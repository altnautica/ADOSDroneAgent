# Board YAML readers for the display overlay installer.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# ----------------------------------------------------------------------------
# Display id auto-pick from board YAML
# ----------------------------------------------------------------------------

# Read the `type` of a display entry from a board's HAL YAML. Scoped to
# the top-level `displays:` block (the YAML also carries an unrelated
# compute `supported:` key) and keyed on the display `id`. Pure awk so
# this works on a fresh BSP without PyYAML. Echoes the type string (e.g.
# "spi-lcd") or nothing when the board / display / type can't be found.
display_type_from_yaml() {
    local board="$1" display="$2"
    local yaml="${REPO_ROOT}/src/ados/hal/boards/${board}.yaml"
    [ -f "${yaml}" ] || return 0
    awk -v want="${display}" '
        /^displays:/ { in_displays = 1; next }
        # Any other top-level key (column 0, non-space) closes the block.
        in_displays && /^[^[:space:]]/ { in_displays = 0 }
        in_displays {
            # New list item: "    - id: <value>"
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                cur_id = line
                next
            }
            if (cur_id == want && $0 ~ /^[[:space:]]*type:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*type:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                print line
                exit
            }
        }
    ' "${yaml}" 2>/dev/null
}

# ----------------------------------------------------------------------------
# Per-panel attribute readers from the board YAML
# ----------------------------------------------------------------------------

# Read an arbitrary scalar key (controller / touch_chip / ...) of a
# display entry, scoped to the top-level displays: block and keyed on the
# display id. Pure awk; mirrors display_type_from_yaml's block scoping so
# it works on a fresh BSP without PyYAML.
display_key_from_yaml() {
    local board="$1" display="$2" key="$3"
    local yaml="${REPO_ROOT}/src/ados/hal/boards/${board}.yaml"
    [ -f "${yaml}" ] || return 0
    awk -v want="${display}" -v key="${key}" '
        /^displays:/ { in_displays = 1; next }
        in_displays && /^[^[:space:]]/ { in_displays = 0 }
        in_displays {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                cur_id = line
                next
            }
            if (cur_id == want && $0 ~ ("^[[:space:]]*" key ":[[:space:]]*")) {
                line = $0
                sub(("^[[:space:]]*" key ":[[:space:]]*"), "", line)
                gsub(/[[:space:]]/, "", line)
                print line
                exit
            }
        }
    ' "${yaml}" 2>/dev/null
}

# Echo the id of the FIRST display in a board's YAML whose type matches
# ``want_type`` (e.g. "hdmi-touch"), or nothing when none is declared. Same
# block scoping as display_type_from_yaml. Pure awk so it works on a fresh
# BSP without PyYAML.
first_display_of_type() {
    local board="$1" want_type="$2"
    local yaml="${REPO_ROOT}/src/ados/hal/boards/${board}.yaml"
    [ -f "${yaml}" ] || return 0
    awk -v want="${want_type}" '
        /^displays:/ { in_displays = 1; next }
        in_displays && /^[^[:space:]]/ { in_displays = 0 }
        in_displays {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                cur_id = line
                next
            }
            if ($0 ~ /^[[:space:]]*type:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*type:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                if (line == want) { print cur_id; exit }
            }
        }
    ' "${yaml}" 2>/dev/null
}

# Read a scalar key from the nested ``touch:`` block of a display entry
# (e.g. the HDMI-touch bounds x_min / swap_xy). Scoped to the display item by
# id, then to its ``touch:`` sub-block by indentation. Echoes the value or
# nothing. Pure awk; no PyYAML dependency on a fresh BSP.
display_touch_key_from_yaml() {
    local board="$1" display="$2" key="$3"
    local yaml="${REPO_ROOT}/src/ados/hal/boards/${board}.yaml"
    [ -f "${yaml}" ] || return 0
    awk -v want="${display}" -v key="${key}" '
        function indent(s,   n) { n = 0; while (substr(s, n + 1, 1) == " ") n++; return n }
        /^displays:/ { in_displays = 1; next }
        in_displays && /^[^[:space:]]/ { in_displays = 0 }
        in_displays {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                cur_id = line
                in_touch = 0
                next
            }
            if (cur_id == want && $0 ~ /^[[:space:]]*touch:[[:space:]]*$/) {
                in_touch = 1
                touch_indent = indent($0)
                next
            }
            if (in_touch) {
                # A line at or shallower than the touch: key closes the block.
                if ($0 ~ /[^[:space:]]/ && indent($0) <= touch_indent) {
                    in_touch = 0
                } else if ($0 ~ ("^[[:space:]]*" key ":[[:space:]]*")) {
                    line = $0
                    sub(("^[[:space:]]*" key ":[[:space:]]*"), "", line)
                    gsub(/[[:space:]]/, "", line)
                    print line
                    exit
                }
            }
        }
    ' "${yaml}" 2>/dev/null
}
