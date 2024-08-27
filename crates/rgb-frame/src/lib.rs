// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2024 Oxide Computer Company

use std::mem::MaybeUninit;
use std::num::NonZeroUsize;

#[derive(Clone, Copy)]
pub struct Spec {
    /// Width of Frame in pixels
    pub width: NonZeroUsize,
    /// Height of Frame in pixels
    pub height: NonZeroUsize,
    /// Width (in bytes) of each row of pixels.
    ///
    /// May be larger than `width * bytes_per_pixel` in order to better align
    /// pixel data in memory.
    pub stride: NonZeroUsize,
    pub fourcc: FourCC,
}
impl Spec {
    pub fn new(width: usize, height: usize, fourcc: FourCC) -> Self {
        Self {
            width: NonZeroUsize::new(width).expect("width is non-zero"),
            height: NonZeroUsize::new(height).expect("height is non-zero"),
            stride: unsafe {
                // Safety: height and width have already been checked for zero
                NonZeroUsize::new_unchecked(
                    width
                        .checked_mul(height)
                        .expect("stride does not overflow"),
                )
            },
            fourcc,
        }
    }
}

/// A frame of pixel data and accompanying metadata
pub struct Frame {
    spec: Spec,
    data: Vec<u8>,
}

impl Frame {
    /// Create a new Frame, sized based on the provided [Spec]
    ///
    /// The contents of the pixel buffer for this frame will be initialized with
    /// all zeroes.
    pub fn new(spec: Spec) -> Self {
        let (mut data, stride) = Self::allocate_for_spec(&spec);
        data.resize_with(data.capacity(), Default::default);
        let spec = Spec { stride, ..spec };

        Self { spec, data }
    }

    /// Create a few Frame, sized based on the provided [Spec], with its pixel
    /// contents initalized via the `populate` function.
    ///
    /// The responsibility is left to the caller to properly initalize the
    /// entire [`MaybeUninit<u8>`] slice provided to the `populate` argument.  The
    /// stride length of that buffer is provided as the second argument.
    pub fn new_uninit<F>(spec: Spec, populate: F) -> Self
    where
        F: FnOnce(&mut [MaybeUninit<u8>], NonZeroUsize),
    {
        let (mut data, stride) = Self::allocate_for_spec(&spec);
        let spare = data.spare_capacity_mut();
        populate(spare, stride);
        unsafe {
            data.set_len(data.capacity());
        };
        Self { spec: Spec { stride, ..spec }, data }
    }

    fn allocate_for_spec(spec: &Spec) -> (Vec<u8>, NonZeroUsize) {
        let bytepp = spec.fourcc.bytes_per_pixel();

        let line_sz = bytepp
            .get()
            .checked_mul(spec.width.get())
            .expect("line size does not overflow");

        // TODO: align allocate for SIMD ops
        let stride = line_sz;
        let buf = Vec::with_capacity(line_sz * spec.height.get());

        (buf, NonZeroUsize::new(stride).expect("stride is non-zero"))
    }

    /// Get the [Spec] for this frame.
    pub fn spec(&self) -> Spec {
        self.spec
    }

    /// Access to the raw pixel bytes of this frame
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Mutable access to the raw pixel bytes of this frame
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Convert between recognized 4-byte pixel formats
    pub fn convert(&mut self, target: FourCC) {
        let source = self.spec.fourcc;
        if source == target {
            return;
        }

        self.spec.fourcc = target;

        let source_rgba = source.le_idx_rgba();
        let target_rgba = target.le_idx_rgba();

        if source_rgba == target_rgba && !target.has_alpha() {
            // order is already the same, and the new format does not need the
            // alpha channel populated
            return;
        }

        // TODO: rub some SIMD on this, when possible
        for pixel in self.data.chunks_exact_mut(4) {
            let red = pixel[source_rgba.0];
            let green = pixel[source_rgba.1];
            let blue = pixel[source_rgba.2];
            // TODO: alpha assumed to be 100% for now
            let alpha = 0xff;

            pixel[target_rgba.0] = red;
            pixel[target_rgba.1] = green;
            pixel[target_rgba.2] = blue;
            pixel[target_rgba.3] = alpha;
        }
    }
}

#[derive(
    Copy,
    Clone,
    Eq,
    PartialEq,
    Debug,
    strum::FromRepr,
    strum::EnumString,
    strum::Display,
    strum::VariantNames,
)]
#[repr(u32)]
#[strum(serialize_all = "UPPERCASE")]
pub enum FourCC {
    /// x:R:G:B `\[` 31:0 `\]` little endian
    XR24 = 0x34325258,
    /// R:G:B:x `\[` 31:0 `\]` little endian
    RX24 = 0x34325852,
    /// x:B:G:R `\[` 31:0 `\]` little endian
    XB24 = 0x34325842,
    /// B:G:R:x `\[` 31:0 `\]` little endian
    BX24 = 0x34324258,

    /// A:R:G:B `\[` 31:0 `\]` little endian
    AR24 = 0x34325241,
    /// R:G:B:A `\[` 31:0 `\]` little endian
    RA24 = 0x34324152,
    /// A:B:G:R `\[` 31:0 `\]` little endian
    AB24 = 0x34324142,
    /// B:G:R:A `\[` 31:0 `\]` little endian
    BA24 = 0x34324241,
}
impl FourCC {
    /// Does this FourCC contain an alpha channel?
    pub const fn has_alpha(self) -> bool {
        matches!(self, Self::AR24 | Self::RA24 | Self::AB24 | Self::BA24)
    }
    /// Returns the (little-endian) byte index of red/green/blue/alpha
    /// components (respectively) in a pixel of this format
    pub const fn le_idx_rgba(self) -> (usize, usize, usize, usize) {
        match self {
            FourCC::XR24 | FourCC::AR24 => (2, 1, 0, 3),
            FourCC::RX24 | FourCC::RA24 => (3, 2, 1, 0),
            FourCC::BX24 | FourCC::BA24 => (1, 2, 3, 0),
            FourCC::XB24 | FourCC::AB24 => (0, 1, 2, 3),
        }
    }
    pub const fn bytes_per_pixel(self) -> NonZeroUsize {
        // Our existing definitions are all 4-byte formats
        // SAFETY: it's a constant
        unsafe { NonZeroUsize::new_unchecked(4) }
    }
}
