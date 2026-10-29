//! Duplicate object elimination (images, fonts, font programs and other streams).

use std::collections::HashMap;

use lopdf::{Document, Object, ObjectId};
use sha2::{Digest, Sha256};

use crate::util::{decode_stream, remap_document};

#[derive(Default, Debug)]
pub struct DedupStats {
    pub merged: usize,
}

/// Dictionary types that are safe to merge when identical.
const MERGEABLE_DICT_TYPES: &[&[u8]] = &[b"Font", b"FontDescriptor", b"ExtGState", b"Encoding"];
/// Keys that describe the encoding rather than the decoded content.
const ENCODING_KEYS: &[&[u8]] = &[b"Length", b"Filter", b"DecodeParms", b"DL"];

/// Canonical serialization (dictionary keys sorted) for hashing.
fn canon(obj: &Object, out: &mut Vec<u8>, skip: &[&[u8]]) {
    match obj {
        Object::Null => out.extend_from_slice(b"n"),
        Object::Boolean(b) => out.extend_from_slice(if *b { b"T" } else { b"F" }),
        Object::Integer(i) => {
            out.push(b'i');
            out.extend_from_slice(&i.to_le_bytes());
        }
        Object::Real(r) => {
            // Normalize so 1 and 1.0 compare equal.
            if r.fract() == 0.0 && r.abs() < 1e15 {
                out.push(b'i');
                out.extend_from_slice(&(*r as i64).to_le_bytes());
            } else {
                out.push(b'r');
                out.extend_from_slice(&r.to_le_bytes());
            }
        }
        Object::Name(n) => {
            out.push(b'/');
            out.extend_from_slice(&(n.len() as u64).to_le_bytes());
            out.extend_from_slice(n);
        }
        Object::String(s, _) => {
            out.push(b'(');
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s);
        }
        Object::Array(a) => {
            out.push(b'[');
            out.extend_from_slice(&(a.len() as u64).to_le_bytes());
            for o in a {
                canon(o, out, &[]);
            }
        }
        Object::Dictionary(d) => {
            let mut keys: Vec<&Vec<u8>> = d
                .iter()
                .map(|(k, _)| k)
                .filter(|k| !skip.contains(&k.as_slice()))
                .collect();
            keys.sort();
            out.push(b'<');
            out.extend_from_slice(&(keys.len() as u64).to_le_bytes());
            for k in keys {
                canon(&Object::Name(k.clone()), out, &[]);
                canon(d.get(k).unwrap(), out, &[]);
            }
        }
        Object::Stream(s) => {
            out.push(b'S');
            canon(&Object::Dictionary(s.dict.clone()), out, skip);
        }
        Object::Reference((n, g)) => {
            out.push(b'R');
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(&g.to_le_bytes());
        }
    }
}

fn key_for(obj: &Object) -> Option<[u8; 32]> {
    let mut buf = Vec::new();
    match obj {
        Object::Stream(s) => {
            if matches!(s.dict.get_type().ok(), Some(b"XRef") | Some(b"ObjStm")) {
                return None;
            }
            match decode_stream(s) {
                Some(data) => {
                    canon(obj, &mut buf, ENCODING_KEYS);
                    let mut h = Sha256::new();
                    h.update(b"decoded");
                    h.update(&buf);
                    h.update((data.len() as u64).to_le_bytes());
                    h.update(&data);
                    Some(h.finalize().into())
                }
                None => {
                    canon(obj, &mut buf, &[b"Length"]);
                    let mut h = Sha256::new();
                    h.update(b"raw");
                    h.update(&buf);
                    h.update((s.content.len() as u64).to_le_bytes());
                    h.update(&s.content);
                    Some(h.finalize().into())
                }
            }
        }
        Object::Dictionary(d) => {
            let t = d.get_type().ok()?;
            if !MERGEABLE_DICT_TYPES.contains(&t) {
                return None;
            }
            canon(obj, &mut buf, &[]);
            Some(Sha256::digest(&buf).into())
        }
        Object::Array(a) => {
            // Indirect colour-space arrays such as [/ICCBased 5 0 R].
            if !matches!(a.first(), Some(Object::Name(_))) {
                return None;
            }
            canon(obj, &mut buf, &[]);
            Some(Sha256::digest(&buf).into())
        }
        _ => None,
    }
}

fn encoded_size(obj: &Object) -> usize {
    match obj {
        Object::Stream(s) => s.content.len(),
        _ => 0,
    }
}

/// Merge identical objects until a fixed point is reached (so that e.g. two images become
/// identical once their SMasks have been merged).
pub fn dedup(doc: &mut Document) -> DedupStats {
    let mut stats = DedupStats::default();
    for _ in 0..16 {
        let mut groups: HashMap<[u8; 32], Vec<ObjectId>> = HashMap::new();
        for (id, obj) in &doc.objects {
            if let Some(k) = key_for(obj) {
                groups.entry(k).or_default().push(*id);
            }
        }
        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for (_, ids) in groups {
            if ids.len() < 2 {
                continue;
            }
            let keep = *ids
                .iter()
                .min_by_key(|id| (encoded_size(&doc.objects[id]), **id))
                .unwrap();
            for id in ids {
                if id != keep {
                    map.insert(id, keep);
                }
            }
        }
        if map.is_empty() {
            break;
        }
        stats.merged += map.len();
        remap_document(doc, &map);
        for id in map.keys() {
            doc.objects.remove(id);
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    fn image(
        doc: &mut Document,
        pixels: &[u8],
        smask: Option<ObjectId>,
        compress: bool,
    ) -> ObjectId {
        let mut d = dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 2,
            "BitsPerComponent" => 8, "ColorSpace" => if smask.is_some() { "DeviceRGB" } else { "DeviceGray" },
        };
        if let Some(m) = smask {
            d.set("SMask", m);
        }
        let mut s = Stream::new(d, pixels.to_vec());
        if compress {
            crate::util::set_flate_content(&mut s, pixels);
        }
        doc.add_object(s)
    }

    #[test]
    fn merges_images_after_their_smasks() {
        let mut doc = Document::with_version("1.7");
        let m1 = image(&mut doc, &[1, 2, 3, 4], None, false);
        let m2 = image(&mut doc, &[1, 2, 3, 4], None, true); // same pixels, different encoding
        let rgb = [9u8; 12];
        let i1 = image(&mut doc, &rgb, Some(m1), true);
        let i2 = image(&mut doc, &rgb, Some(m2), true);
        let other = image(&mut doc, &[5, 6, 7, 8], None, false);
        let xobj = dictionary! { "A" => i1, "B" => i2, "C" => other };
        let page = doc.add_object(
            dictionary! { "Type" => "Page", "Resources" => dictionary! { "XObject" => xobj } },
        );
        let pages = doc.add_object(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        );
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", cat);

        let stats = dedup(&mut doc);
        assert_eq!(stats.merged, 2);
        let res = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap();
        let x = res.get(b"XObject").unwrap().as_dict().unwrap();
        assert_eq!(x.get(b"A").unwrap(), x.get(b"B").unwrap());
        assert_ne!(x.get(b"A").unwrap(), x.get(b"C").unwrap());
        // The smaller encoding of the SMask is kept.
        let kept = doc
            .get_object(x.get(b"A").unwrap().as_reference().unwrap())
            .unwrap()
            .as_stream()
            .unwrap();
        let mask = kept.dict.get(b"SMask").unwrap().as_reference().unwrap();
        assert_eq!(mask, m1);
    }
}
