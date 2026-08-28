//! Dependency-free PNG encoder for `control_plane`'s `CaptureFrame` verb
//! (`frame_capture::render_frame_png` is the one caller).
//!
//! This crate's own delivery notes for `CaptureFrame` looked for an
//! already-resolved image/PNG crate to reuse first (`image`/`png` are
//! real, already-compiled entries in this workspace's `Cargo.lock` --
//! pulled in transitively through `uzor-text` -> `uzor`) and deliberately
//! did not add either as a direct dependency of this crate: this
//! module's own byte-for-byte PNG assembly is small enough (one 8-bit
//! RGBA image, one IDAT chunk, no filtering beyond "None", no real
//! DEFLATE compression) that hand-rolling it stays well inside "an
//! existing rasteriser/dependency serves" territory, and doing so keeps
//! `gate4agent-tui`'s own `Cargo.toml` exactly as it was before this verb
//! existed -- see this crate's global instructions on not introducing a
//! new dependency without it being asked for outright.
//!
//! ## No real compression, on purpose
//!
//! [`deflate_stored`] emits ONLY RFC 1951 "stored" (type `00`) blocks --
//! literal bytes, no Huffman coding, no LZ77 back-references. A real
//! DEFLATE compressor is a substantial piece of machinery on its own (a
//! second rasteriser-scale undertaking for a debug capture verb); a
//! stored block is legal, trivial, and any conforming PNG decoder reads
//! it exactly like a compressed one. The cost is that the PNG on disk
//! lands close to its RAW pixel size rather than a real compressor's
//! typical result -- acceptable ONLY because `control_plane::capture_
//! frame`'s own wire reply carries a filesystem PATH to this file, never
//! the bytes themselves (see that function's own doc comment): nothing
//! here is bounded by `CONTROL_RESPONSE_MAX_BYTES`, so an uncompressed
//! multi-megabyte file on disk costs nothing this wire has to answer for.
//!
//! ## Format emitted
//!
//! 8-bit-per-channel truecolor-with-alpha (PNG colour type 6), filter
//! type `None` (0) on every scanline, single IDAT chunk, no palette, no
//! interlacing -- the simplest legal shape for an RGBA8 source buffer,
//! and the one every mainstream decoder (a browser, an image viewer, a
//! Python/Rust/JS test harness) reads without any special-casing.

/// One PNG file's worth of bytes for an 8-bit RGBA `width`x`height`
/// image. `rgba.len()` MUST equal `width * height * 4` -- every caller in
/// this crate builds `rgba` from a canvas already sized exactly that way
/// (`frame_capture::paint_frame_rgba`), so this is an invariant of the
/// call site, not a value a caller could get wrong by surprise; checked
/// with `debug_assert_eq!` rather than an `Err` return because a mismatch
/// here is a programming error in THIS crate, never a malformed-input
/// case a remote caller could trigger (nothing on `control_plane`'s wire
/// ever supplies `rgba` bytes).
pub(crate) fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    debug_assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4, "rgba buffer must be exactly width*height*4 bytes");

    let mut out = Vec::with_capacity(rgba.len() + rgba.len() / 8 + 64);
    out.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // Bit depth 8, colour type 6 (truecolor + alpha), compression method
    // 0 (the only one PNG defines), filter method 0 (the only one PNG
    // defines -- distinct from the PER-SCANLINE filter TYPE byte below),
    // interlace method 0 (no interlacing).
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);

    let stride = (width as usize) * 4;
    let mut filtered = Vec::with_capacity((height as usize) * (stride + 1));
    for row in 0..height as usize {
        // Filter type `None` (0): every scanline is prefixed with this one
        // byte, then its raw pixel bytes verbatim -- no per-pixel
        // prediction. Simplest legal choice; this image is not being
        // optimized for size (see this module's own "No real compression"
        // doc section above), so there is nothing a fancier filter would
        // buy back here.
        filtered.push(0);
        let start = row * stride;
        filtered.extend_from_slice(&rgba[start..start + stride]);
    }

    let mut zlib_stream = Vec::with_capacity(filtered.len() + 16);
    // zlib header: CMF=0x78 (DEFLATE, 32K window), FLG=0x01 (FCHECK makes
    // `(0x78 << 8 | 0x01) % 31 == 0`; FLEVEL bits both `0` -- "fastest
    // compression", which is the honest description of "no compression at
    // all"). No preset dictionary (FDICT bit unset).
    zlib_stream.extend_from_slice(&[0x78, 0x01]);
    zlib_stream.extend_from_slice(&deflate_stored(&filtered));
    zlib_stream.extend_from_slice(&adler32(&filtered).to_be_bytes());
    write_chunk(&mut out, b"IDAT", &zlib_stream);

    write_chunk(&mut out, b"IEND", &[]);
    out
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Writes one length-prefixed, CRC-suffixed PNG chunk (`length | type |
/// data | crc32(type ++ data)`, per the PNG spec) straight onto `out`.
fn write_chunk(out: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// RFC 1951 section 3.2.4 "stored" (uncompressed) blocks, split at the
/// format's own 65535-byte-per-block ceiling -- see this module's own "No
/// real compression" doc section for why stored blocks, not a real
/// DEFLATE compressor. Each block's 3-bit header (`BFINAL` + `BTYPE=00`)
/// is written as a single whole byte rather than 3 packed bits: RFC 1951
/// requires a stored block's header to be immediately followed by
/// byte-alignment padding before `LEN`/`NLEN`, and every block boundary
/// in THIS stream is already byte-aligned (the very first block starts at
/// a fresh byte, and every stored block's own content ends on one, by
/// construction) -- so writing the header as one byte with its own upper
/// 5 bits simply left at `0` (the decoder ignores exactly those bits as
/// padding) is byte-for-byte the same stream a bit-packing writer would
/// produce here, just without needing a bit-level writer at all.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    const MAX_STORED_BLOCK_LEN: usize = 0xFFFF;
    let mut out = Vec::with_capacity(data.len() + (data.len() / MAX_STORED_BLOCK_LEN + 1) * 5);
    let mut offset = 0;
    loop {
        let remaining = data.len() - offset;
        let chunk_len = remaining.min(MAX_STORED_BLOCK_LEN);
        let is_final = offset + chunk_len >= data.len();
        out.push(if is_final { 0x01 } else { 0x00 });
        let len = chunk_len as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + chunk_len]);
        offset += chunk_len;
        if is_final {
            break;
        }
    }
    out
}

/// Standard Adler-32 (RFC 1950's own zlib trailer checksum) -- the
/// straightforward per-byte accumulator, not the NMAX-batched variant
/// zlib's own reference implementation uses to avoid overflow on 32-bit
/// accumulators: `a`/`b` are reduced modulo 65521 every byte here, so
/// neither can ever exceed it, and a full-terminal frame (at most a few
/// million bytes) is nowhere near where the batched version's own
/// performance advantage would matter for a capture verb this crate
/// expects to run once per operator request, not per render frame.
fn adler32(bytes: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in bytes {
        a = (a + u32::from(byte)) % MOD_ADLER;
        b = (b + a) % MOD_ADLER;
    }
    (b << 16) | a
}

/// CRC-32 (ISO 3309 / ITU-T V.42, the exact variant PNG chunk trailers
/// require), reflected polynomial `0xEDB88320`, table-driven. The table
/// is built once at COMPILE time ([`CRC32_TABLE`], a `const fn` -- stable
/// since well before this crate's own MSRV), not lazily at first use,
/// since every PNG this module ever encodes needs it and there is no
/// "never called" path worth deferring the cost for.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut byte = 0usize;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// Minimal PNG chunk/zlib/stored-DEFLATE reader for THIS module's own
/// output -- not a general-purpose PNG decoder (it would panic on a real
/// compressed IDAT, an interlaced image, or a filter type other than
/// `None`, none of which [`encode_png`] ever produces). Exists so this
/// module's own tests -- and `frame_capture`'s own dimension test, which
/// calls this directly (`#[cfg(test)]` applies crate-wide under `cargo
/// test`, so `pub(crate)` here is reachable from that module's own test
/// code) -- can verify [`encode_png`]'s output round-trips byte-for-byte
/// without pulling in an external PNG-decoding dependency just for test
/// assertions (see this module's own top doc comment on why this crate
/// hand-rolls the encoder in the first place; a decoder brought in only
/// to check the encoder's own homework would undercut that same
/// reasoning). Defined at module scope rather than inside `mod tests`
/// below, specifically so it stays reachable from another module's own
/// test code -- a function nested inside a PRIVATE `mod tests` is not,
/// regardless of its own declared visibility.
#[cfg(test)]
pub(crate) fn decode_for_test(png: &[u8]) -> (u32, u32, Vec<u8>) {
    assert_eq!(&png[0..8], &PNG_SIGNATURE, "missing PNG signature");
    let mut offset = 8;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut idat = Vec::new();
    loop {
        let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
        let chunk_type = &png[offset + 4..offset + 8];
        let data = &png[offset + 8..offset + 8 + length];
        match chunk_type {
            b"IHDR" => {
                width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                assert_eq!(data[8], 8, "expected bit depth 8");
                assert_eq!(data[9], 6, "expected colour type 6 (RGBA)");
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        offset += 8 + length + 4;
    }

    // zlib wrapper: 2-byte header, N-byte deflate stream, 4-byte
    // Adler-32 trailer.
    let deflate_data = &idat[2..idat.len() - 4];
    let mut raw = Vec::new();
    let mut position = 0;
    loop {
        let header = deflate_data[position];
        position += 1;
        let len = u16::from_le_bytes([deflate_data[position], deflate_data[position + 1]]) as usize;
        position += 4; // LEN (2 bytes) + NLEN (2 bytes)
        raw.extend_from_slice(&deflate_data[position..position + len]);
        position += len;
        if header & 1 == 1 {
            break;
        }
    }

    let stride = (width as usize) * 4;
    let mut rgba = Vec::with_capacity((height as usize) * stride);
    for row in 0..height as usize {
        let start = row * (stride + 1);
        assert_eq!(raw[start], 0, "expected filter type None on every scanline");
        rgba.extend_from_slice(&raw[start + 1..start + 1 + stride]);
    }
    (width, height, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_image_round_trips_through_encode_and_decode() {
        let width = 3;
        let height = 2;
        let rgba: Vec<u8> = (0..width * height)
            .flat_map(|index| [index as u8 * 10, index as u8 * 20, index as u8 * 30, 255])
            .collect();
        let png = encode_png(width, height, &rgba);
        let (decoded_width, decoded_height, decoded_rgba) = decode_for_test(&png);
        assert_eq!(decoded_width, width);
        assert_eq!(decoded_height, height);
        assert_eq!(decoded_rgba, rgba);
    }

    #[test]
    fn output_starts_with_the_png_signature_and_ends_with_iend() {
        let rgba = vec![0u8; 4];
        let png = encode_png(1, 1, &rgba);
        assert!(png.starts_with(&PNG_SIGNATURE));
        assert!(png.ends_with(b"IEND\xaeB`\x82"), "IEND chunk must carry its own fixed CRC");
    }

    /// A frame wide/tall enough that its filtered scanline data exceeds
    /// one stored block's own 65535-byte ceiling -- proves
    /// [`deflate_stored`] actually splits into multiple blocks (only the
    /// LAST of which is marked final) rather than silently truncating or
    /// panicking past that limit.
    #[test]
    fn a_frame_larger_than_one_stored_block_still_round_trips() {
        let width = 400;
        let height = 200; // 400*4 + 1 = 1601 bytes/row * 200 rows = 320_200 bytes, several blocks
        let rgba = vec![7u8; (width * height * 4) as usize];
        let png = encode_png(width, height, &rgba);
        let (decoded_width, decoded_height, decoded_rgba) = decode_for_test(&png);
        assert_eq!(decoded_width, width);
        assert_eq!(decoded_height, height);
        assert_eq!(decoded_rgba, rgba);
    }

    #[test]
    fn crc32_matches_the_known_reference_vector_for_the_ascii_bytes_123456789() {
        // The canonical CRC-32/ISO-HDLC test vector every implementation
        // of this variant is checked against.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_a_known_reference_vector() {
        // "Wikipedia" -> 0x11E60398, a widely-cited Adler-32 worked example.
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
