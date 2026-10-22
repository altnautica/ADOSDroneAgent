//! Little-endian cursor helpers over fixed-length payload slices. Callers
//! check the payload length first, so these never index out of range.

pub(crate) struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.buf[self.pos..self.pos + b.len()].copy_from_slice(b);
        self.pos += b.len();
    }
    pub(crate) fn zeros(&mut self, n: usize) {
        self.buf[self.pos..self.pos + n].fill(0);
        self.pos += n;
    }
    pub(crate) fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    pub(crate) fn i8(&mut self, v: i8) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn i16(&mut self, v: i16) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn i32(&mut self, v: i32) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn f32(&mut self, v: f32) {
        self.bytes(&v.to_le_bytes());
    }
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub(crate) fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        out
    }
    pub(crate) fn skip(&mut self, n: usize) {
        self.pos += n;
    }
    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
    pub(crate) fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }
    pub(crate) fn i8(&mut self) -> i8 {
        i8::from_le_bytes(self.take())
    }
    pub(crate) fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
    pub(crate) fn i16(&mut self) -> i16 {
        i16::from_le_bytes(self.take())
    }
    pub(crate) fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    pub(crate) fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.take())
    }
    pub(crate) fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    pub(crate) fn f32(&mut self) -> f32 {
        f32::from_le_bytes(self.take())
    }
}
