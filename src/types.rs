pub struct ReadOp<'a> {
    pub offset: usize,
    pub buf: &'a mut [u8],
}
impl<'a> ReadOp<'a> {
    pub fn new(offset: usize, buf: &'a mut [u8]) -> Self {
        Self { offset, buf }
    }
}

pub struct WriteOp<'a> {
    pub offset: usize,
    pub buf: &'a [u8],
}
impl<'a> WriteOp<'a> {
    pub fn new(offset: usize, buf: &'a [u8]) -> Self {
        Self { offset, buf }
    }
}

pub enum RWOp<'a, 'b> {
    Read(&'a mut ReadOp<'b>),
    Write(&'a WriteOp<'b>),
}

pub struct GuestAddr(pub u64);
