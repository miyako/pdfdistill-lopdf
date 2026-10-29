//! Image recompression (Flate -> JPEG) and downsampling based on effective resolution.

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object, ObjectId};

use crate::content::Usage;
use crate::util::{
    decode_stream, deflate_best, filters, is_fully_decodable, num, reference_index, resolve,
    set_flate_compressed,
};

#[derive(Default, Debug)]
pub struct ImageStats {
    pub to_jpeg: usize,
    pub reflated: usize,
    pub downsampled: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

pub struct ImageOptions {
    pub quality: u8,
    pub max_dpi: u32,
    /// Allow lossy JPEG encoding.
    pub jpeg: bool,
}

/// Number of colour components if the colour space is one we can safely re-encode.
/// Returns (components, jpeg_ok).
fn components(doc: &Document, cs: Option<&Object>) -> Option<(usize, bool)> {
    let cs = resolve(doc, cs?);
    match cs {
        Object::Name(n) => match n.as_slice() {
            b"DeviceGray" | b"G" => Some((1, true)),
            b"DeviceRGB" | b"RGB" => Some((3, true)),
            b"DeviceCMYK" | b"CMYK" => Some((4, false)),
            _ => None,
        },
        Object::Array(a) if !a.is_empty() => match resolve(doc, &a[0]).as_name().ok()? {
            b"ICCBased" => {
                let s = resolve(doc, a.get(1)?).as_stream().ok()?;
                match s.dict.get(b"N").ok().and_then(num)? as i64 {
                    1 => Some((1, true)),
                    3 => Some((3, true)),
                    4 => Some((4, false)),
                    _ => None,
                }
            }
            b"CalGray" => Some((1, true)),
            b"CalRGB" => Some((3, true)),
            _ => None,
        },
        _ => None,
    }
}

fn default_decode(doc: &Document, d: &lopdf::Dictionary, n: usize) -> bool {
    match d.get(b"Decode").map(|o| resolve(doc, o)) {
        Err(_) | Ok(Object::Null) => true,
        Ok(Object::Array(a)) => {
            a.len() == 2 * n
                && a.iter()
                    .enumerate()
                    .all(|(i, v)| num(resolve(doc, v)) == Some(if i % 2 == 0 { 0.0 } else { 1.0 }))
        }
        _ => false,
    }
}

/// Area-averaging resampler (box filter with fractional coverage), separable.
pub fn resample(src: &[u8], w: usize, h: usize, n: usize, nw: usize, nh: usize) -> Vec<u8> {
    fn weights(src: usize, dst: usize) -> Vec<Vec<(usize, f32)>> {
        let scale = src as f64 / dst as f64;
        (0..dst)
            .map(|i| {
                let start = i as f64 * scale;
                let end = ((i + 1) as f64 * scale).min(src as f64);
                let mut v = Vec::new();
                let mut x = start.floor() as usize;
                while (x as f64) < end && x < src {
                    let lo = start.max(x as f64);
                    let hi = end.min(x as f64 + 1.0);
                    if hi > lo {
                        v.push((x, ((hi - lo) / (end - start)) as f32));
                    }
                    x += 1;
                }
                v
            })
            .collect()
    }
    let wx = weights(w, nw);
    let wy = weights(h, nh);
    // Horizontal pass.
    let mut tmp = vec![0f32; nw * h * n];
    for y in 0..h {
        let row = &src[y * w * n..(y + 1) * w * n];
        for (ox, ws) in wx.iter().enumerate() {
            for c in 0..n {
                let mut acc = 0f32;
                for &(x, wt) in ws {
                    acc += row[x * n + c] as f32 * wt;
                }
                tmp[(y * nw + ox) * n + c] = acc;
            }
        }
    }
    // Vertical pass.
    let mut out = vec![0u8; nw * nh * n];
    for (oy, ws) in wy.iter().enumerate() {
        for i in 0..nw * n {
            let mut acc = 0f32;
            for &(y, wt) in ws {
                acc += tmp[y * nw * n + i] * wt;
            }
            out[oy * nw * n + i] = acc.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

pub fn encode_jpeg(pixels: &[u8], w: usize, h: usize, n: usize, quality: u8) -> Option<Vec<u8>> {
    if w > u16::MAX as usize || h > u16::MAX as usize {
        return None;
    }
    let ct = match n {
        1 => jpeg_encoder::ColorType::Luma,
        3 => jpeg_encoder::ColorType::Rgb,
        _ => return None,
    };
    // Optimized Huffman tables are smaller, but for degenerate (e.g. flat) images they can
    // produce single-code tables that strict decoders reject. Verify by decoding and fall
    // back to the standard tables if needed.
    for optimize in [true, false] {
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, quality);
        enc.set_optimized_huffman_tables(optimize);
        if enc.encode(pixels, w as u16, h as u16, ct).is_err() {
            continue;
        }
        if matches!(decode_jpeg(&out, n), Some((_, dw, dh)) if dw == w && dh == h) {
            return Some(out);
        }
    }
    None
}

fn decode_jpeg(data: &[u8], n: usize) -> Option<(Vec<u8>, usize, usize)> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let out_cs = match n {
        1 => ColorSpace::Luma,
        3 => ColorSpace::RGB,
        _ => return None,
    };
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(out_cs)
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(data, opts);
    dec.decode_headers().ok()?;
    let in_cs = dec.get_input_colorspace()?;
    if in_cs.num_components() != n {
        return None;
    }
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if pixels.len() != w * h * n {
        return None;
    }
    Some((pixels, w, h))
}

pub fn optimize_images(
    doc: &mut Document,
    usage: &Usage,
    opts: &ImageOptions,
    warn: &mut dyn FnMut(String),
) -> ImageStats {
    let mut stats = ImageStats::default();
    let refs = reference_index(doc);

    let mut images: Vec<ObjectId> = Vec::new();
    // child (SMask / Mask stream) -> parents
    let mut mask_parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for (id, obj) in &doc.objects {
        if let Object::Stream(s) = obj {
            if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() == Some(b"Image") {
                images.push(*id);
                for key in [&b"SMask"[..], b"Mask"] {
                    if let Ok(Object::Reference(m)) = s.dict.get(key) {
                        mask_parents.entry(*m).or_default().push(*id);
                    }
                }
            }
        }
    }
    let image_set: HashSet<ObjectId> = images.iter().copied().collect();

    // Largest placed extent, only if every reference is understood.
    let extent_direct = |id: ObjectId| -> Option<(f64, f64)> {
        if !usage.complete || !usage.fully_accounted(refs.get(&id)) {
            return None;
        }
        usage.image_extent.get(&id).copied()
    };
    let extent = |id: ObjectId| -> Option<(f64, f64)> {
        let mut ext = (0f64, 0f64);
        let mut any = false;
        for c in refs.get(&id).into_iter().flatten() {
            if image_set.contains(c)
                && mask_parents
                    .get(&id)
                    .map(|p| p.contains(c))
                    .unwrap_or(false)
            {
                let e = extent_direct(*c)?;
                ext = (ext.0.max(e.0), ext.1.max(e.1));
                any = true;
            } else if !usage.walked.contains(c) || usage.failed.contains(c) {
                return None;
            }
        }
        if let Some(e) = usage.image_extent.get(&id) {
            if !usage.complete {
                return None;
            }
            ext = (ext.0.max(e.0), ext.1.max(e.1));
            any = true;
        }
        any.then_some(ext)
    };

    let mut targets: HashMap<ObjectId, (usize, usize)> = HashMap::new();
    for &id in &images {
        let Some(Object::Stream(s)) = doc.objects.get(&id) else {
            continue;
        };
        let w = s.dict.get(b"Width").ok().and_then(num).unwrap_or(0.0) as usize;
        let h = s.dict.get(b"Height").ok().and_then(num).unwrap_or(0.0) as usize;
        let mut t = (w, h);
        if opts.max_dpi > 0 && w > 0 && h > 0 {
            if let Some((pw, ph)) = extent(id) {
                let md = opts.max_dpi as f64;
                if pw > 0.01 {
                    let dpi = w as f64 * 72.0 / pw;
                    if dpi > md {
                        t.0 = ((w as f64 * md / dpi).round() as usize).max(1);
                    }
                }
                if ph > 0.01 {
                    let dpi = h as f64 * 72.0 / ph;
                    if dpi > md {
                        t.1 = ((h as f64 * md / dpi).round() as usize).max(1);
                    }
                }
            }
        }
        targets.insert(id, t);
    }

    for &id in &images {
        let Some(Object::Stream(s)) = doc.objects.get(&id) else {
            continue;
        };
        let d = &s.dict;
        let is_mask_child = mask_parents.contains_key(&id);
        if matches!(d.get(b"ImageMask"), Ok(Object::Boolean(true))) {
            continue;
        }
        if d.get(b"BitsPerComponent").ok().and_then(num) != Some(8.0) {
            continue;
        }
        if matches!(
            d.get(b"Mask").map(|m| resolve(doc, m)),
            Ok(Object::Array(_))
        ) {
            continue; // colour-key masking needs exact sample values
        }
        if d.has(b"SMaskInData") || d.has(b"Alternates") || d.has(b"OPI") {
            continue;
        }
        let Some((n, cs_jpeg_ok)) = (if is_mask_child && !d.has(b"ColorSpace") {
            Some((1, false))
        } else {
            components(doc, d.get(b"ColorSpace").ok())
        }) else {
            continue;
        };
        if !default_decode(doc, d, n) {
            continue;
        }
        let w = d.get(b"Width").ok().and_then(num).unwrap_or(0.0) as usize;
        let h = d.get(b"Height").ok().and_then(num).unwrap_or(0.0) as usize;
        if w == 0 || h == 0 || w.saturating_mul(h).saturating_mul(n) > crate::util::MAX_DECODED {
            continue;
        }
        let (tw, th) = targets.get(&id).copied().unwrap_or((w, h));
        let downsample = tw < w || th < h;
        let jpeg_ok = opts.jpeg && cs_jpeg_ok && !is_mask_child;

        let f = filters(s).unwrap_or_default();
        let is_dct = f.len() == 1 && f[0] == b"DCTDecode";
        let pixels = if is_fully_decodable(s) {
            match decode_stream(s) {
                Some(mut p) if p.len() >= w * h * n => {
                    p.truncate(w * h * n);
                    p
                }
                _ => continue,
            }
        } else if is_dct && downsample && jpeg_ok {
            match decode_jpeg(&s.content, n) {
                Some((p, jw, jh)) if jw == w && jh == h => p,
                _ => continue,
            }
        } else {
            continue;
        };

        let (pixels, ow, oh) = if downsample {
            (resample(&pixels, w, h, n, tw, th), tw, th)
        } else {
            (pixels, w, h)
        };
        let flate = deflate_best(&pixels);
        let jpeg = if jpeg_ok {
            encode_jpeg(&pixels, ow, oh, n, opts.quality)
        } else {
            None
        };
        let old_len = s.content.len();
        let use_jpeg = matches!(&jpeg, Some(j) if j.len() < flate.len());
        let new_len = if use_jpeg {
            jpeg.as_ref().unwrap().len()
        } else {
            flate.len()
        };
        if new_len >= old_len {
            continue;
        }

        let Some(Object::Stream(s)) = doc.objects.get_mut(&id) else {
            continue;
        };
        if use_jpeg {
            s.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
            s.dict.remove(b"DecodeParms");
            s.set_content(jpeg.unwrap());
            stats.to_jpeg += 1;
        } else {
            set_flate_compressed(s, flate);
            stats.reflated += 1;
        }
        if downsample {
            s.dict.set("Width", ow as i64);
            s.dict.set("Height", oh as i64);
            stats.downsampled += 1;
        }
        stats.bytes_before += old_len;
        stats.bytes_after += new_len;
    }
    if !usage.complete && opts.max_dpi > 0 {
        warn("content analysis incomplete; images were not downsampled".into());
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_averages_blocks() {
        let src = [0u8, 100, 200, 255, 0, 100, 200, 255];
        let out = resample(&src, 4, 2, 1, 2, 1);
        assert_eq!(out, vec![50, 228]);
    }

    #[test]
    fn resample_fractional_and_rgb() {
        let src: Vec<u8> = (0..3 * 3 * 3).map(|i| (i * 9) as u8).collect();
        let out = resample(&src, 3, 3, 3, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 3);
        let flat = vec![77u8; 5 * 7 * 3];
        assert!(resample(&flat, 5, 7, 3, 2, 3).iter().all(|&v| v == 77));
    }

    #[test]
    fn jpeg_roundtrip_dimensions() {
        let px: Vec<u8> = (0..64 * 32 * 3).map(|i| ((i * 7) % 251) as u8).collect();
        let j = encode_jpeg(&px, 64, 32, 3, 75).unwrap();
        let (p, w, h) = decode_jpeg(&j, 3).unwrap();
        assert_eq!((w, h, p.len()), (64, 32, px.len()));
        let g: Vec<u8> = (0..40 * 24).map(|i| (i % 256) as u8).collect();
        let j = encode_jpeg(&g, 40, 24, 1, 50).unwrap();
        let (p, w, h) = decode_jpeg(&j, 1).unwrap();
        assert_eq!((w, h, p.len()), (40, 24, g.len()));
    }
}
