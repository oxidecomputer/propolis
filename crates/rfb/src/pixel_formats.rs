// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2022 Oxide Computer Company

//! Pixel Formats
//!
//! The pixel format data structure is specified in section 7.4 of RFC 6143. The data structure
//! describes how large a pixel is in bits, how many bits of the pixel are used for describing
//! color, and how colors are encoded in the pixel: either as a color specification or a color map,
//! with a color specification being the most common.
//!
//! The color specification format describes which bits in the pixel represent each color (red,
//! green, and blue), the max value of each color, and the endianness of the pixel. The location of
//! each color is described by a shift, in which the shift represents how many shifts are needed to
//! get the color value to the least significant bit (that is, how many right shifts are needed).
//!
//! For example, consider the 32-bit pixel value 0x01020304 with a depth of 24. Let's say the pixel
//! format has a red shift of 0, green shift of 8, blue shift of 16, all colors have a max value of
//! 255, and the host is little-endian. This is the pixel format little-endian xBGR.
//!
//! So to get the value of each color, we would do:
//! - red = (0x01020304 >> 0) & 255 = 0x04
//! - blue = (0x01020304 >> 8) & 255 = 0x03
//! - green = (0x01020304 >> 16) & 255 = 0x02
//!
//! This is relatively straightforward when considering a single pixel format. But an RFB server
//! must be able to translate between pixel formats, including translating between hosts of
//! different endianness. Further, it is convenient to represent pixels using a vector of bytes
//! instead of a vector of n-bit values, and transformations on pixels are done for groups of bytes
//! representing a pixel, rather than a single pixel value. But thinking about pixels in this
//! representation can be tricky, as the pixel format describes shifts, which operate the same on a
//! value regardless of endianness, but code operating on a byte vector must be endian-aware.
//!
//! If we think about the same pixel before as a byte vector, we would have the following
//! representation: [ 0x04, 0x03, 0x02, 0x01 ]. Note that the bytes are in reverse order from the
//! value above because the host is little-endian, so the least-significant byte (0x04) is first.
//!
//! So to get the value of each color, we would index into the pixel based on the shift. A shift of
//! 0 indicates the color is at the least significant byte (the first byte, byte 0 for
//!   little-endian pixels), a shift of 8 is the second least significant byte (1), and so on:
//! - red = pixel\[0\] & 255 = 0x04
//! - green = pixel\[1\] & 255 = 0x03
//! - blue = pixel\[2\] & 255 = 0x02
//!
//! Since the RFB server is considering pixels that might be from little-endian or big-endian hosts
//! though, consider if the same byte vector came from an RGBx big endian pixel. In that case, the
//! least significant byte is byte 3 and the most significant byte is byte 0. So the color values
//! for this vector would be:
//! - red = pixel\[3\] & 255 = 0x01
//! - green = pixel\[2\] & 255 = 0x02
//! - blue = pixel\[1\] & 255 = 0x03
//!

#[derive(Debug, thiserror::Error)]
pub enum PixelFormatError {
    #[error("unsupported or unknown fourcc: 0x{0:x}")]
    UnsupportedFourCc(u32),
}

///  Utility functions and constants related to fourcc codes.
///
/// Fourcc is a 4-byte ASCII code representing a pixel format. For example, the value
/// 0x34325258 is '42RX' in ASCII (34='4', 32='2', 52='R', and 58='X'). This code maps to the pixel
/// format 32-bit little-endian xRGB.
///
/// A good reference for mapping common fourcc codes to their corresponding pixel formats is the
/// drm_fourcc.h header file in the linux source code.
pub mod fourcc {
    use super::{rgb_888, PixelFormatError};
    use crate::rfb::{ColorFormat, ColorSpecification, PixelFormat};

    // XXX: it might make sense to turn fourcc values into a type (such as an enum or collection of
    // enums)
    pub const FOURCC_XR24: u32 = 0x34325258; // little-endian xRGB, 8:8:8:8
    pub const FOURCC_RX24: u32 = 0x34325852; // little-endian RGBx, 8:8:8:8
    pub const FOURCC_BX24: u32 = 0x34325842; // little-endian BGRx, 8:8:8:8
    pub const FOURCC_XB24: u32 = 0x34324258; // little-endian xBGR, 8:8:8:8

    pub fn fourcc_to_pixel_format(fourcc: u32) -> Result<PixelFormat, PixelFormatError> {
        match fourcc {
            // little-endian xRGB
            FOURCC_XR24 => Ok(PixelFormat {
                bits_per_pixel: rgb_888::BITS_PER_PIXEL,
                depth: rgb_888::DEPTH,
                big_endian: false,
                color_spec: ColorSpecification::ColorFormat(ColorFormat {
                    red_max: rgb_888::MAX_VALUE,
                    green_max: rgb_888::MAX_VALUE,
                    blue_max: rgb_888::MAX_VALUE,
                    red_shift: rgb_888::BITS_PER_COLOR * 2,
                    green_shift: rgb_888::BITS_PER_COLOR * 1,
                    blue_shift: rgb_888::BITS_PER_COLOR * 0,
                }),
            }),

            // little-endian RGBx
            FOURCC_RX24 => Ok(PixelFormat {
                bits_per_pixel: rgb_888::BITS_PER_PIXEL,
                depth: rgb_888::DEPTH,
                big_endian: false,
                color_spec: ColorSpecification::ColorFormat(ColorFormat {
                    red_max: rgb_888::MAX_VALUE,
                    green_max: rgb_888::MAX_VALUE,
                    blue_max: rgb_888::MAX_VALUE,
                    red_shift: rgb_888::BITS_PER_COLOR * 3,
                    green_shift: rgb_888::BITS_PER_COLOR * 2,
                    blue_shift: rgb_888::BITS_PER_COLOR * 1,
                }),
            }),

            // little-endian BGRx
            FOURCC_BX24 => Ok(PixelFormat {
                bits_per_pixel: rgb_888::BITS_PER_PIXEL,
                depth: rgb_888::DEPTH,
                big_endian: false,
                color_spec: ColorSpecification::ColorFormat(ColorFormat {
                    red_max: rgb_888::MAX_VALUE,
                    green_max: rgb_888::MAX_VALUE,
                    blue_max: rgb_888::MAX_VALUE,
                    red_shift: rgb_888::BITS_PER_COLOR * 1,
                    green_shift: rgb_888::BITS_PER_COLOR * 2,
                    blue_shift: rgb_888::BITS_PER_COLOR * 3,
                }),
            }),

            // little-endian xBGR
            FOURCC_XB24 => Ok(PixelFormat {
                bits_per_pixel: rgb_888::BITS_PER_PIXEL,
                depth: rgb_888::DEPTH,
                big_endian: false,
                color_spec: ColorSpecification::ColorFormat(ColorFormat {
                    red_max: rgb_888::MAX_VALUE,
                    green_max: rgb_888::MAX_VALUE,
                    blue_max: rgb_888::MAX_VALUE,
                    red_shift: rgb_888::BITS_PER_COLOR * 0,
                    green_shift: rgb_888::BITS_PER_COLOR * 1,
                    blue_shift: rgb_888::BITS_PER_COLOR * 2,
                }),
            }),

            v => Err(PixelFormatError::UnsupportedFourCc(v)),
        }
    }
}

/// Utility functions for 32-bit RGB pixel formats, with 8-bits used per color.
pub mod rgb_888 {
    use crate::rfb::{ColorSpecification, PixelFormat};

    pub const BYTES_PER_PIXEL: usize = 4;
    pub const BITS_PER_PIXEL: u8 = 32;

    /// Number of bits used for color in a pixel
    pub const DEPTH: u8 = 24;

    /// Number of bits used for a single color value
    pub const BITS_PER_COLOR: u8 = 8;

    /// Max value for a given color
    pub const MAX_VALUE: u16 = 255;

    /// Returns true if a shift as specified in a pixel format is valid for rgb888 formats.
    pub fn valid_shift(shift: u8) -> bool {
        shift == 0 || shift == 8 || shift == 16 || shift == 24
    }

    /// Returns the byte index into a 4-byte pixel vector for a given color shift, accounting for endianness.
    pub fn color_shift_to_index(shift: u8, big_endian: bool) -> usize {
        assert!(valid_shift(shift));

        if big_endian {
            ((DEPTH - shift) / BITS_PER_COLOR) as usize
        } else {
            (shift / BITS_PER_COLOR) as usize
        }
    }

    /// Returns the index of the unused byte (the only byte not representing R, G, or B).
    pub fn unused_index(r: usize, g: usize, b: usize) -> usize {
        (3 + 2 + 1) - r - g - b
    }

    /// Given a set of red/green/blue shifts from a pixel format and its endianness, determine
    /// which byte index in a 4-byte vector representing a pixel maps to which color.
    ///
    /// For example, for the shifts red=0, green=8, blue=16, and a little-endian format, the
    /// indices would be red=0, green=1, blue=2, and x=3.
    pub fn rgbx_index(
        red_shift: u8,
        green_shift: u8,
        blue_shift: u8,
        big_endian: bool,
    ) -> (usize, usize, usize, usize) {
        let r = color_shift_to_index(red_shift, big_endian);
        let g = color_shift_to_index(green_shift, big_endian);
        let b = color_shift_to_index(blue_shift, big_endian);
        let x = unused_index(r, g, b);

        (r, g, b, x)
    }

    /// Translate between RGB888 formats. The input and output format must both be RGB888.
    pub fn transform(pixels: &Vec<u8>, input: &PixelFormat, output: &PixelFormat) -> Vec<u8> {
        assert!(input.is_rgb_888());
        assert!(output.is_rgb_888());

        //let mut buf = Vec::with_capacity(pixels.len());
        //buf.resize(pixels.len(), 0x0u8);
        let mut buf = vec![0; pixels.len()];

        let (ir, ig, ib, ix) = match &input.color_spec {
            ColorSpecification::ColorFormat(cf) => rgbx_index(
                cf.red_shift,
                cf.green_shift,
                cf.blue_shift,
                input.big_endian,
            ),
            ColorSpecification::ColorMap(_) => {
                unreachable!();
            }
        };

        let (or, og, ob, ox) = match &output.color_spec {
            ColorSpecification::ColorFormat(cf) => rgbx_index(
                cf.red_shift,
                cf.green_shift,
                cf.blue_shift,
                output.big_endian,
            ),
            ColorSpecification::ColorMap(_) => {
                unreachable!();
            }
        };

        let mut i = 0;
        while i < pixels.len() {
            // Get the value for each color from the input...
            let r = pixels[i + ir];
            let g = pixels[i + ig];
            let b = pixels[i + ib];
            let x = pixels[i + ix];

            // and assign it to the right spot in the output pixel
            buf[i + or] = r;
            buf[i + og] = g;
            buf[i + ob] = b;
            buf[i + ox] = x;

            i += 4;
        }

        buf
    }
}

#[cfg(test)]
mod tests {
    use crate::pixel_formats::rgb_888::{color_shift_to_index, rgbx_index};

    use super::{fourcc, rgb_888::transform};

    #[test]
    fn test_color_shift_to_index() {
        assert_eq!(color_shift_to_index(0, false), 0);
        assert_eq!(color_shift_to_index(8, false), 1);
        assert_eq!(color_shift_to_index(16, false), 2);
        assert_eq!(color_shift_to_index(24, false), 3);

        assert_eq!(color_shift_to_index(0, true), 3);
        assert_eq!(color_shift_to_index(8, true), 2);
        assert_eq!(color_shift_to_index(16, true), 1);
        assert_eq!(color_shift_to_index(24, true), 0);
    }

    #[test]
    fn test_rgbx_index() {
        assert_eq!(rgbx_index(0, 8, 16, false), (0, 1, 2, 3));
        assert_eq!(rgbx_index(0, 8, 16, true), (3, 2, 1, 0));

        assert_eq!(rgbx_index(8, 16, 24, false), (1, 2, 3, 0));
        assert_eq!(rgbx_index(8, 16, 24, true), (2, 1, 0, 3));

        assert_eq!(rgbx_index(0, 16, 24, false), (0, 2, 3, 1));
        assert_eq!(rgbx_index(0, 16, 24, true), (3, 1, 0, 2));

        assert_eq!(rgbx_index(8, 16, 24, false), (1, 2, 3, 0));
        assert_eq!(rgbx_index(8, 16, 24, true), (2, 1, 0, 3));

        assert_eq!(rgbx_index(0, 24, 8, false), (0, 3, 1, 2));
        assert_eq!(rgbx_index(0, 24, 8, true), (3, 0, 2, 1));
    }

    #[test]
    fn test_rgb888_transform() {
        let pixels = vec![0u8, 1u8, 2u8, 3u8];

        // little-endian xRGB
        let xrgb_le = fourcc::fourcc_to_pixel_format(fourcc::FOURCC_XR24).unwrap();

        // little-endian RGBx
        let rgbx_le = fourcc::fourcc_to_pixel_format(fourcc::FOURCC_RX24).unwrap();

        // little-endian BGRx
        let bgrx_le = fourcc::fourcc_to_pixel_format(fourcc::FOURCC_BX24).unwrap();

        // little-endian xBGR
        let xbgr_le = fourcc::fourcc_to_pixel_format(fourcc::FOURCC_XB24).unwrap();

        // same pixel format
        assert_eq!(transform(&pixels, &xrgb_le, &xrgb_le), pixels);
        assert_eq!(transform(&pixels, &rgbx_le, &rgbx_le), pixels);
        assert_eq!(transform(&pixels, &bgrx_le, &bgrx_le), pixels);
        assert_eq!(transform(&pixels, &xbgr_le, &xbgr_le), pixels);

        // little-endian xRGB -> little-endian RGBx
        //  B  G  R  x            x  B  G  R
        // [0, 1, 2, 3]       -> [3, 0, 1, 2]
        let p2 = vec![3u8, 0u8, 1u8, 2u8];
        assert_eq!(transform(&pixels, &xrgb_le, &rgbx_le), p2);

        // little-endian RGBx -> little-endian xRGB
        //  x  B  G  R            B  G  R  x
        // [0, 1, 2, 3]       -> [1, 2, 3, 0]
        let p3 = vec![1u8, 2u8, 3u8, 0u8];
        assert_eq!(transform(&pixels, &rgbx_le, &xrgb_le), p3);

        // little-endian xRGB -> little-endian BGRx
        //  B  G  R  x            x  R  G  B
        // [0, 1, 2, 3]       -> [3, 2, 1, 0]
        let p4 = vec![3u8, 2u8, 1u8, 0u8];
        assert_eq!(transform(&pixels, &xrgb_le, &bgrx_le), p4);
        // little-endian BGRx -> little-endian xRGB
        //  x  R  G  B            B  G  R  x
        // [0, 1, 2, 3]       -> [3, 2, 1, 0]
        assert_eq!(transform(&pixels, &bgrx_le, &xrgb_le), p4);

        // little-endian BGRx -> little-endian xBGR
        //  x  R  G  B            R  G  B  x
        // [0, 1, 2, 3]       -> [1, 2, 3, 0]
        let p5 = vec![1u8, 2u8, 3u8, 0u8];
        assert_eq!(transform(&pixels, &bgrx_le, &xbgr_le), p5);
    }
}
