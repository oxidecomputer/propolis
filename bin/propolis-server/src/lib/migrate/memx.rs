use crate::migrate::codec;

// The bitmap data structure uses a single bit to represent
// each 4KiB page frame in the [start, end) GPA range, but we
// use [start, end) pages instead of a start address and an
// upper bound determined by the size of the bitmap, so we need
// to verify that the size of the bitmap corresponds to the
// given range.
//
// Why not use a simpler, base and bound representation?
// There is a problem: because the minimum width data type
// useful for the bitmap is an eight bit byte, it represents
// up to eight pages.  Since there is no constraint that the
// range associated with any given bitmap be eight-page
// aligned, we must be able to handle the bitmap containing
// the case where the last entry in the bitmap contains bits
// corresponding to up to seven bits beyond the range.
//
// Fortunately, we can declare that any attempt to create an
// invalid bitmap by software is a programming error, and
// assert against it.  Similarly, any invalid received
// bitmap sent from a remote Propolis is an error, and we
// can abort.  This code enforces these rules as invariants.

// Validates the parameters from a fetch, offer, or xfer.
pub(crate) fn validate_bitmap(start: u64, end: u64, bits: &[u8]) -> bool {
    if start % 4096 != 0 || end % 4096 != 0 {
        return false;
    }
    if end <= start {
        return false;
    }
    let npages = ((end - start) / 4096) as usize;
    let npages_bitmap = bits.len() * 8;
    if npages_bitmap < npages || (npages_bitmap - npages) >= 8 {
        return false;
    }
    if npages_bitmap != npages {
        let last_bits = npages_bitmap - npages;
        let mask = !0u8 << (8 - last_bits);
        let last_byte = bits[bits.len() - 1];
        if last_byte & mask != 0 {
            return false;
        }
    }
    true
}

/// Creates an offer message for a range of physical
/// addresses and a bitmap.
pub(crate) fn make_mem_offer(
    start_gpa: u64,
    end_gpa: u64,
    bitmap: &[u8],
) -> codec::Message {
    assert!(validate_bitmap(start_gpa, end_gpa, bitmap));
    codec::Message::MemOffer(start_gpa, end_gpa, bitmap.into())
}

pub(crate) fn make_mem_fetch(
    start_gpa: u64,
    end_gpa: u64,
    bitmap: &[u8],
) -> codec::Message {
    assert!(validate_bitmap(start_gpa, end_gpa, bitmap));
    codec::Message::MemFetch(start_gpa, end_gpa, bitmap.into())
}

pub(crate) fn make_mem_xfer(
    start_gpa: u64,
    end_gpa: u64,
    bitmap: &[u8],
) -> codec::Message {
    assert!(validate_bitmap(start_gpa, end_gpa, bitmap));
    codec::Message::MemXfer(start_gpa, end_gpa, bitmap.into())
}

#[cfg(test)]
mod memx_test {
    use super::*;
    use crate::migrate::codec;

    #[test]
    fn make_mem_offer_simple() {
        let bitmap = &[0b1010_1001u8];
        let msg = make_mem_offer(0, 8 * 4096, bitmap);
        let expected = vec![0b1010_1001u8];
        assert!(matches!(msg, codec::Message::MemOffer(0, s, v)
            if s == 8 * 4096 && v == expected));
    }

    #[test]
    fn make_mem_xfer_short() {
        let bitmap = &[0b0011_1111u8];
        let msg = make_mem_xfer(0, 6 * 4096, bitmap);
        let expected = vec![0b0011_1111u8];
        assert!(matches!(msg, codec::Message::MemXfer(0, s, v)
            if s == 6 * 4096 && v == expected));
    }

    #[test]
    #[should_panic]
    fn make_mem_fetch_too_long_fails() {
        let bitmap = &[0b1100_0000u8];
        let _msg = make_mem_fetch(0, 7 * 4096, bitmap);
    }
}
