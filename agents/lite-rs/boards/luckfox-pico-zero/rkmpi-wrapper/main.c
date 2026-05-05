/*
 * RKMPI hardware H.264 encoder wrapper for the lite ADOS Drone Agent.
 *
 * Written against the published RK_MPI_VENC_* API surface from the
 * Rockchip vendor library shipped with the Luckfox Pico Zero SDK.
 * Compiled with the uclibc cross-toolchain (arm-rockchip830-linux-
 * uclibcgnueabihf-gcc) to keep the libc compatible with the vendor
 * .so. The Rust parent stays musl-static; this wrapper bridges the
 * libc gap with a clean process boundary.
 *
 * Wire format (matches crates/ados-video/src/rkmpi_subprocess.rs):
 *
 *   parent -> child (stdin):
 *     +--------------+----------------------+
 *     | u32 BE       | msgpack body         |
 *     | length       | SubprocessRequest    |
 *     +--------------+----------------------+
 *
 *   child -> parent (stdout):
 *     same shape, body is SubprocessResponse
 *
 * Lifecycle:
 *   1. Read Start request, init RKMPI sys + create VENC channel.
 *   2. Send Ready response.
 *   3. Loop: pull encoded packets via RK_MPI_VENC_GetStream, frame
 *      and emit on stdout. Watch stdin for a Stop request between
 *      pulls; on EOF or Stop, break.
 *   4. Tear down VENC channel + RKMPI sys.
 *
 * Stderr is the parent's stderr (inherited) so vendor library
 * diagnostics land in the agent's journalctl alongside structured
 * tracing output.
 *
 * NOTE: this wrapper exercises the encoder side of the pipeline. The
 * input frame source (ISP -> VPSS -> VENC binding) lives in the
 * vendor SDK's example apps; once on-hardware bringup confirms the
 * encode-only path round-trips through msgpack cleanly, the binding
 * to a live ISP source replaces the synthetic-frame path here.
 */

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <sys/select.h>

#include "rk_common.h"
#include "rk_comm_venc.h"
#include "rk_mpi_mb.h"
#include "rk_mpi_sys.h"
#include "rk_mpi_venc.h"

/* ------------------------------------------------------------------ */
/* Wire framing                                                       */
/* ------------------------------------------------------------------ */

#define MAX_FRAME_BYTES (4 * 1024 * 1024)  /* matches the Rust cap */

/* Read exactly n bytes from fd, or return -1 on EOF/error. */
static int read_exact(int fd, void *buf, size_t n) {
    size_t off = 0;
    while (off < n) {
        ssize_t r = read(fd, (char *)buf + off, n - off);
        if (r == 0) return -1;        /* EOF */
        if (r < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        off += (size_t)r;
    }
    return 0;
}

/* Write exactly n bytes to fd, or return -1 on error. */
static int write_exact(int fd, const void *buf, size_t n) {
    size_t off = 0;
    while (off < n) {
        ssize_t w = write(fd, (const char *)buf + off, n - off);
        if (w < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        off += (size_t)w;
    }
    return 0;
}

/* Read one length-prefixed msgpack body from stdin into *out (caller
 * owns + frees). Returns 0 on success, -1 on EOF/error/oversized. */
static int read_framed(uint8_t **out, uint32_t *len_out) {
    uint8_t lenbuf[4];
    if (read_exact(0, lenbuf, 4) < 0) return -1;
    uint32_t len = ((uint32_t)lenbuf[0] << 24) | ((uint32_t)lenbuf[1] << 16) |
                   ((uint32_t)lenbuf[2] << 8) | (uint32_t)lenbuf[3];
    if (len == 0 || len > MAX_FRAME_BYTES) return -1;
    uint8_t *body = (uint8_t *)malloc(len);
    if (!body) return -1;
    if (read_exact(0, body, len) < 0) {
        free(body);
        return -1;
    }
    *out = body;
    *len_out = len;
    return 0;
}

/* Write a length-prefixed msgpack body to stdout. */
static int write_framed(const uint8_t *body, uint32_t len) {
    uint8_t lenbuf[4] = {
        (uint8_t)((len >> 24) & 0xff),
        (uint8_t)((len >> 16) & 0xff),
        (uint8_t)((len >> 8) & 0xff),
        (uint8_t)(len & 0xff),
    };
    if (write_exact(1, lenbuf, 4) < 0) return -1;
    if (write_exact(1, body, len) < 0) return -1;
    return 0;
}

/* ------------------------------------------------------------------ */
/* Minimal msgpack helpers                                            */
/* ------------------------------------------------------------------ */

/*
 * The Rust side uses rmp-serde with #[serde(tag="kind", rename_all=
 * "snake_case")]. Each request/response is a fixed-shape map:
 *
 *   Start: {"kind":"start","width":u32,"height":u32,"fps":u32,
 *           "bitrate_kbps":u32,"keyframe_interval_secs":u32}
 *   Stop:  {"kind":"stop"}
 *
 *   Ready: {"kind":"ready"}
 *   Frame: {"kind":"frame","is_keyframe":bool,"pts_ms":u64,
 *           "bytes":bin}
 *   Error: {"kind":"error","message":str}
 *
 * We encode/decode by hand to avoid pulling msgpack-c into the uclibc
 * build. The shapes are stable; rmp-serde uses the canonical
 * map-with-string-keys form.
 */

/* Encode a fixmap header with N keys. N must be <= 15. */
static size_t enc_fixmap(uint8_t *p, uint8_t n) {
    p[0] = 0x80 | (n & 0x0f);
    return 1;
}

/* Encode a fixstr header for an N-byte UTF-8 string. N must be <= 31. */
static size_t enc_fixstr(uint8_t *p, const char *s) {
    size_t n = strlen(s);
    if (n > 31) return 0;
    p[0] = 0xa0 | (uint8_t)(n & 0x1f);
    memcpy(p + 1, s, n);
    return 1 + n;
}

/* Encode a u32 / u64 (positive integer). Picks the smallest fitting
 * msgpack type so the output matches what rmp-serde produces. */
static size_t enc_u64(uint8_t *p, uint64_t v) {
    if (v <= 0x7f) {
        p[0] = (uint8_t)v;
        return 1;
    }
    if (v <= 0xff) {
        p[0] = 0xcc;
        p[1] = (uint8_t)v;
        return 2;
    }
    if (v <= 0xffff) {
        p[0] = 0xcd;
        p[1] = (uint8_t)(v >> 8);
        p[2] = (uint8_t)v;
        return 3;
    }
    if (v <= 0xffffffffULL) {
        p[0] = 0xce;
        p[1] = (uint8_t)(v >> 24);
        p[2] = (uint8_t)(v >> 16);
        p[3] = (uint8_t)(v >> 8);
        p[4] = (uint8_t)v;
        return 5;
    }
    p[0] = 0xcf;
    p[1] = (uint8_t)(v >> 56);
    p[2] = (uint8_t)(v >> 48);
    p[3] = (uint8_t)(v >> 40);
    p[4] = (uint8_t)(v >> 32);
    p[5] = (uint8_t)(v >> 24);
    p[6] = (uint8_t)(v >> 16);
    p[7] = (uint8_t)(v >> 8);
    p[8] = (uint8_t)v;
    return 9;
}

/* Encode a bool. */
static size_t enc_bool(uint8_t *p, int b) {
    p[0] = b ? 0xc3 : 0xc2;
    return 1;
}

/* Encode a binary blob with the bin8 / bin16 / bin32 prefix. */
static size_t enc_bin(uint8_t *p, const uint8_t *data, size_t n) {
    if (n <= 0xff) {
        p[0] = 0xc4;
        p[1] = (uint8_t)n;
        memcpy(p + 2, data, n);
        return 2 + n;
    }
    if (n <= 0xffff) {
        p[0] = 0xc5;
        p[1] = (uint8_t)(n >> 8);
        p[2] = (uint8_t)n;
        memcpy(p + 3, data, n);
        return 3 + n;
    }
    p[0] = 0xc6;
    p[1] = (uint8_t)(n >> 24);
    p[2] = (uint8_t)(n >> 16);
    p[3] = (uint8_t)(n >> 8);
    p[4] = (uint8_t)n;
    memcpy(p + 5, data, n);
    return 5 + n;
}

/* Decode a Start request. Returns 0 on success, -1 on shape mismatch. */
struct start_config {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate_kbps;
    uint32_t keyframe_interval_secs;
};

/* Tag-pull: scan the body for "width", "height", etc. Each key is a
 * fixstr; the matching value is decoded as a u32. We don't enforce
 * key ordering — rmp-serde with serde(tag) emits stable order, but
 * the decoder is order-agnostic for resilience.
 *
 * Returns 0 on success, -1 if mandatory keys missing or types wrong.
 */
static int find_u32(const uint8_t *body, uint32_t len, const char *key,
                    uint32_t *out) {
    size_t kn = strlen(key);
    for (uint32_t i = 0; i + 1 + kn < len; i++) {
        /* fixstr header byte */
        if (body[i] != (0xa0 | (uint8_t)kn)) continue;
        if (memcmp(body + i + 1, key, kn) != 0) continue;
        size_t v = i + 1 + kn;
        if (v >= len) return -1;
        uint8_t tag = body[v];
        if (tag <= 0x7f) {
            *out = tag;
            return 0;
        }
        if (tag == 0xcc && v + 1 < len) {
            *out = body[v + 1];
            return 0;
        }
        if (tag == 0xcd && v + 2 < len) {
            *out = ((uint32_t)body[v + 1] << 8) | body[v + 2];
            return 0;
        }
        if (tag == 0xce && v + 4 < len) {
            *out = ((uint32_t)body[v + 1] << 24) |
                   ((uint32_t)body[v + 2] << 16) |
                   ((uint32_t)body[v + 3] << 8) | body[v + 4];
            return 0;
        }
        return -1;
    }
    return -1;
}

static int decode_start(const uint8_t *body, uint32_t len,
                        struct start_config *cfg) {
    if (find_u32(body, len, "width", &cfg->width) < 0) return -1;
    if (find_u32(body, len, "height", &cfg->height) < 0) return -1;
    if (find_u32(body, len, "fps", &cfg->fps) < 0) return -1;
    if (find_u32(body, len, "bitrate_kbps", &cfg->bitrate_kbps) < 0) return -1;
    if (find_u32(body, len, "keyframe_interval_secs",
                 &cfg->keyframe_interval_secs) < 0) return -1;
    return 0;
}

/* Detect if body is a Stop request. Stop has only one map key: "kind"
 * with value "stop". A scan for the literal substring "stop" suffices
 * because no other variant carries that token. */
static int is_stop(const uint8_t *body, uint32_t len) {
    static const char needle[] = "stop";
    for (uint32_t i = 0; i + 4 <= len; i++) {
        if (memcmp(body + i, needle, 4) == 0) return 1;
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* Response emitters                                                  */
/* ------------------------------------------------------------------ */

static int emit_ready(void) {
    uint8_t body[16];
    size_t off = 0;
    off += enc_fixmap(body + off, 1);
    off += enc_fixstr(body + off, "kind");
    off += enc_fixstr(body + off, "ready");
    return write_framed(body, (uint32_t)off);
}

static int emit_error(const char *msg) {
    size_t mlen = strlen(msg);
    if (mlen > 240) mlen = 240;
    uint8_t *body = (uint8_t *)malloc(64 + mlen);
    if (!body) return -1;
    size_t off = 0;
    off += enc_fixmap(body + off, 2);
    off += enc_fixstr(body + off, "kind");
    off += enc_fixstr(body + off, "error");
    off += enc_fixstr(body + off, "message");
    body[off++] = (uint8_t)(0xa0 | (mlen & 0x1f));
    memcpy(body + off, msg, mlen);
    off += mlen;
    int rc = write_framed(body, (uint32_t)off);
    free(body);
    return rc;
}

static int emit_frame(int is_keyframe, uint64_t pts_ms,
                      const uint8_t *data, size_t dlen) {
    /* Allocate generously: header + 5-byte bin32 prefix + payload. */
    size_t cap = 128 + dlen;
    uint8_t *body = (uint8_t *)malloc(cap);
    if (!body) return -1;
    size_t off = 0;
    off += enc_fixmap(body + off, 4);
    off += enc_fixstr(body + off, "kind");
    off += enc_fixstr(body + off, "frame");
    off += enc_fixstr(body + off, "is_keyframe");
    off += enc_bool(body + off, is_keyframe);
    off += enc_fixstr(body + off, "pts_ms");
    off += enc_u64(body + off, pts_ms);
    off += enc_fixstr(body + off, "bytes");
    off += enc_bin(body + off, data, dlen);
    int rc = write_framed(body, (uint32_t)off);
    free(body);
    return rc;
}

/* ------------------------------------------------------------------ */
/* RKMPI lifecycle                                                    */
/* ------------------------------------------------------------------ */

#define VENC_CHN 0

static int venc_init(const struct start_config *cfg) {
    RK_S32 ret = RK_MPI_SYS_Init();
    if (ret != RK_SUCCESS) {
        emit_error("RK_MPI_SYS_Init failed");
        return -1;
    }

    VENC_CHN_ATTR_S attr;
    memset(&attr, 0, sizeof(attr));
    attr.stVencAttr.enType = RK_VIDEO_ID_AVC;
    attr.stVencAttr.u32MaxPicWidth = cfg->width;
    attr.stVencAttr.u32MaxPicHeight = cfg->height;
    attr.stVencAttr.u32PicWidth = cfg->width;
    attr.stVencAttr.u32PicHeight = cfg->height;
    attr.stVencAttr.u32VirWidth = cfg->width;
    attr.stVencAttr.u32VirHeight = cfg->height;
    attr.stVencAttr.u32StreamBufCnt = 4;
    attr.stVencAttr.u32BufSize = cfg->width * cfg->height * 3 / 2;
    attr.stVencAttr.enPixelFormat = RK_FMT_YUV420SP;

    attr.stRcAttr.enRcMode = VENC_RC_MODE_H264CBR;
    attr.stRcAttr.stH264Cbr.u32Gop =
        cfg->fps * (cfg->keyframe_interval_secs ? cfg->keyframe_interval_secs : 2);
    attr.stRcAttr.stH264Cbr.u32BitRate = cfg->bitrate_kbps;
    attr.stRcAttr.stH264Cbr.fr32DstFrameRateDen = 1;
    attr.stRcAttr.stH264Cbr.fr32DstFrameRateNum = cfg->fps;
    attr.stRcAttr.stH264Cbr.u32SrcFrameRateDen = 1;
    attr.stRcAttr.stH264Cbr.u32SrcFrameRateNum = cfg->fps;

    ret = RK_MPI_VENC_CreateChn(VENC_CHN, &attr);
    if (ret != RK_SUCCESS) {
        emit_error("RK_MPI_VENC_CreateChn failed");
        RK_MPI_SYS_Exit();
        return -1;
    }

    VENC_RECV_PIC_PARAM_S recv;
    memset(&recv, 0, sizeof(recv));
    recv.s32RecvPicNum = -1;  /* unbounded */
    ret = RK_MPI_VENC_StartRecvFrame(VENC_CHN, &recv);
    if (ret != RK_SUCCESS) {
        emit_error("RK_MPI_VENC_StartRecvFrame failed");
        RK_MPI_VENC_DestroyChn(VENC_CHN);
        RK_MPI_SYS_Exit();
        return -1;
    }
    return 0;
}

static void venc_teardown(void) {
    RK_MPI_VENC_StopRecvFrame(VENC_CHN);
    RK_MPI_VENC_DestroyChn(VENC_CHN);
    RK_MPI_SYS_Exit();
}

/* H.264 keyframe sniff: first NAL unit type after the start code. */
static int frame_is_keyframe(const uint8_t *p, size_t n) {
    /* Search for 00 00 00 01 or 00 00 01 start code. */
    for (size_t i = 0; i + 4 < n; i++) {
        if (p[i] == 0x00 && p[i + 1] == 0x00 &&
            ((p[i + 2] == 0x00 && p[i + 3] == 0x01) ||
             (p[i + 2] == 0x01))) {
            size_t off = (p[i + 2] == 0x01) ? i + 3 : i + 4;
            if (off >= n) return 0;
            uint8_t nal_type = p[off] & 0x1f;
            /* 5 = IDR slice, 7 = SPS, 8 = PPS — keyframe markers. */
            return nal_type == 5 || nal_type == 7 || nal_type == 8;
        }
    }
    return 0;
}

/* Pull one encoded packet from VENC, frame and emit on stdout.
 * Returns 0 on success, -1 on hard error (caller should exit). */
static int pump_one_packet(void) {
    VENC_STREAM_S stream;
    memset(&stream, 0, sizeof(stream));
    VENC_PACK_S pack;
    memset(&pack, 0, sizeof(pack));
    stream.pstPack = &pack;
    stream.u32PackCount = 1;

    /* 100 ms timeout — long enough not to spin, short enough to
     * notice a Stop request via the inter-pull stdin poll. */
    RK_S32 ret = RK_MPI_VENC_GetStream(VENC_CHN, &stream, 100);
    if (ret == -RK_ERR_VENC_TIMEOUT || ret == RK_ERR_VENC_BUF_EMPTY) {
        return 0;  /* no packet yet, fine */
    }
    if (ret != RK_SUCCESS) {
        emit_error("RK_MPI_VENC_GetStream failed");
        return -1;
    }

    uint8_t *vbuf = (uint8_t *)RK_MPI_MB_Handle2VirAddr(pack.pMbBlk);
    if (!vbuf) {
        RK_MPI_VENC_ReleaseStream(VENC_CHN, &stream);
        emit_error("VENC packet has null vir addr");
        return -1;
    }
    uint8_t *payload = vbuf + pack.u32Offset;
    size_t plen = pack.u32Len;

    int kf = frame_is_keyframe(payload, plen);
    uint64_t pts_ms = (uint64_t)(pack.u64PTS / 1000);  /* PTS is microseconds */

    int rc = emit_frame(kf, pts_ms, payload, plen);
    RK_MPI_VENC_ReleaseStream(VENC_CHN, &stream);
    return rc;
}

/* Non-blocking poll on stdin for an incoming Stop request. Returns
 * 1 if Stop seen, 0 if no data, -1 on EOF/error. */
static int check_stop_nonblocking(void) {
    fd_set rfds;
    FD_ZERO(&rfds);
    FD_SET(0, &rfds);
    struct timeval tv = {.tv_sec = 0, .tv_usec = 0};
    int n = select(1, &rfds, NULL, NULL, &tv);
    if (n <= 0) return 0;

    uint8_t *body = NULL;
    uint32_t len = 0;
    if (read_framed(&body, &len) < 0) {
        if (body) free(body);
        return -1;  /* parent closed stdin */
    }
    int stop = is_stop(body, len);
    free(body);
    return stop ? 1 : 0;
}

/* ------------------------------------------------------------------ */
/* main                                                               */
/* ------------------------------------------------------------------ */

int main(void) {
    /* Disable line buffering on stdin/stdout so framing is exact. */
    setbuf(stdin, NULL);
    setbuf(stdout, NULL);
    signal(SIGPIPE, SIG_IGN);  /* don't crash if parent vanishes */

    /* Step 1: Read Start request. */
    uint8_t *body = NULL;
    uint32_t len = 0;
    if (read_framed(&body, &len) < 0) {
        emit_error("failed to read start request");
        return 1;
    }
    struct start_config cfg;
    if (decode_start(body, len, &cfg) < 0) {
        emit_error("malformed start request");
        free(body);
        return 1;
    }
    free(body);

    /* Step 2: Init VENC. */
    if (venc_init(&cfg) < 0) {
        return 1;  /* error already emitted */
    }

    /* Step 3: Send Ready. */
    if (emit_ready() < 0) {
        venc_teardown();
        return 1;
    }

    /* Step 4: Pump loop. Pull packets, watch for Stop. */
    for (;;) {
        int stop = check_stop_nonblocking();
        if (stop != 0) break;  /* Stop or EOF */
        if (pump_one_packet() < 0) break;
    }

    /* Step 5: Tear down. */
    venc_teardown();
    return 0;
}
