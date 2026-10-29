//! Small helpers shared by the optimization passes.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

/// Upper bound for any single decoded stream (decompression-bomb guard).
pub const MAX_DECODED: usize = 1 << 30;

/// Pseudo object id used for the trailer in reference indexes.
pub const TRAILER_ID: ObjectId = (0, u16::MAX);

/// Filters we can fully decode to raw bytes.
const DECODABLE: &[&[u8]] = &[
    b"FlateDecode",
    b"LZWDecode",
    b"ASCII85Decode",
    b"ASCIIHexDecode",
    b"RunLengthDecode",
];

/// Return the filter chain of a stream (empty if unfiltered), or `None` if malformed.
pub fn filters(stream: &Stream) -> Option<Vec<Vec<u8>>> {
    match stream.dict.get(b"Filter") {
        Err(_) => Some(Vec::new()),
        Ok(Object::Name(n)) => Some(vec![n.clone()]),
        Ok(Object::Array(a)) => a
            .iter()
            .map(|o| o.as_name().ok().map(|n| n.to_vec()))
            .collect(),
        Ok(_) => None,
    }
}

/// True if every filter of the stream can be decoded by [`decode_stream`].
pub fn is_fully_decodable(stream: &Stream) -> bool {
    match filters(stream) {
        Some(f) => f.iter().all(|n| DECODABLE.contains(&n.as_slice())),
        None => false,
    }
}

/// Decode a stream completely and strictly. Returns `None` if the stream uses a filter we
/// cannot decode, or if any part of the data is corrupt (lopdf silently tolerates corrupt
/// Flate data, which would be dangerous to re-encode).
pub fn decode_stream(stream: &Stream) -> Option<Vec<u8>> {
    let f = filters(stream)?;
    if f.is_empty() {
        return Some(stream.content.clone());
    }
    if !f.iter().all(|n| DECODABLE.contains(&n.as_slice())) {
        return None;
    }
    if f.len() == 1 && f[0] == b"FlateDecode" {
        let raw = inflate_strict(&stream.content)?;
        if !has_predictor(stream.dict.get(b"DecodeParms").ok()) {
            return Some(raw);
        }
    }
    stream.decompressed_content_with_limit(MAX_DECODED).ok()
}

fn has_predictor(parms: Option<&Object>) -> bool {
    let check = |d: &Dictionary| {
        d.get(b"Predictor")
            .and_then(|p| p.as_i64())
            .map(|p| p > 1)
            .unwrap_or(false)
    };
    match parms {
        Some(Object::Dictionary(d)) => check(d),
        Some(Object::Array(a)) => a.iter().any(|o| o.as_dict().map(check).unwrap_or(false)),
        _ => false,
    }
}

/// Inflate zlib data, failing on any error.
pub fn inflate_strict(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 3);
    let mut dec = ZlibDecoder::new(data).take(MAX_DECODED as u64 + 1);
    dec.read_to_end(&mut out).ok()?;
    if out.len() > MAX_DECODED {
        return None;
    }
    Some(out)
}

/// Deflate at maximum compression.
pub fn deflate_best(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), Compression::best());
    enc.write_all(data).expect("in-memory write");
    enc.finish().expect("in-memory write")
}

/// Replace stream content with Flate-compressed `raw` bytes.
pub fn set_flate_content(stream: &mut Stream, raw: &[u8]) {
    let data = deflate_best(raw);
    set_flate_compressed(stream, data);
}

pub fn set_flate_compressed(stream: &mut Stream, compressed: Vec<u8>) {
    stream
        .dict
        .set("Filter", Object::Name(b"FlateDecode".to_vec()));
    stream.dict.remove(b"DecodeParms");
    stream.set_content(compressed);
}

/// Follow a reference (with a small loop guard).
pub fn resolve<'a>(doc: &'a Document, mut obj: &'a Object) -> &'a Object {
    for _ in 0..32 {
        match obj {
            Object::Reference(id) => match doc.objects.get(id) {
                Some(o) => obj = o,
                None => return &Object::Null,
            },
            _ => return obj,
        }
    }
    &Object::Null
}

pub fn resolve_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    match resolve(doc, obj) {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

pub fn dict_get<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    dict.get(key)
        .ok()
        .map(|o| resolve(doc, o))
        .filter(|o| !matches!(o, Object::Null))
}

pub fn name_of<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    dict_get(doc, dict, key).and_then(|o| o.as_name().ok())
}

pub fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

pub fn int_of(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<i64> {
    match dict_get(doc, dict, key)? {
        Object::Integer(i) => Some(*i),
        Object::Real(r) => Some(*r as i64),
        _ => None,
    }
}

/// Visit every object reference contained (directly or nested) in `obj`.
pub fn for_each_ref(obj: &Object, f: &mut dyn FnMut(ObjectId)) {
    match obj {
        Object::Reference(id) => f(*id),
        Object::Array(a) => a.iter().for_each(|o| for_each_ref(o, f)),
        Object::Dictionary(d) => d.iter().for_each(|(_, o)| for_each_ref(o, f)),
        Object::Stream(s) => s.dict.iter().for_each(|(_, o)| for_each_ref(o, f)),
        _ => {}
    }
}

/// Rewrite references in `obj` according to `map`.
pub fn remap_refs(obj: &mut Object, map: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if let Some(n) = map.get(id) {
                *id = *n;
            }
        }
        Object::Array(a) => a.iter_mut().for_each(|o| remap_refs(o, map)),
        Object::Dictionary(d) => d.iter_mut().for_each(|(_, o)| remap_refs(o, map)),
        Object::Stream(s) => s.dict.iter_mut().for_each(|(_, o)| remap_refs(o, map)),
        _ => {}
    }
}

pub fn remap_document(doc: &mut Document, map: &HashMap<ObjectId, ObjectId>) {
    if map.is_empty() {
        return;
    }
    for obj in doc.objects.values_mut() {
        remap_refs(obj, map);
    }
    for (_, v) in doc.trailer.iter_mut() {
        remap_refs(v, map);
    }
}

/// Map each referenced object id to the set of top-level objects (containers) referencing it.
/// The trailer is represented by [`TRAILER_ID`].
pub fn reference_index(doc: &Document) -> HashMap<ObjectId, HashSet<ObjectId>> {
    let mut idx: HashMap<ObjectId, HashSet<ObjectId>> = HashMap::new();
    for (id, obj) in &doc.objects {
        for_each_ref(obj, &mut |r| {
            idx.entry(r).or_default().insert(*id);
        });
    }
    for (_, v) in doc.trailer.iter() {
        for_each_ref(v, &mut |r| {
            idx.entry(r).or_default().insert(TRAILER_ID);
        });
    }
    idx
}

/// Remove every object not reachable from the trailer. Returns number removed.
pub fn prune_unreferenced(doc: &mut Document) -> usize {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = Vec::new();
    for (_, v) in doc.trailer.iter() {
        for_each_ref(v, &mut |r| stack.push(r));
    }
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(o) = doc.objects.get(&id) {
            for_each_ref(o, &mut |r| {
                if !seen.contains(&r) {
                    stack.push(r)
                }
            });
        }
    }
    let before = doc.objects.len();
    doc.objects.retain(|id, _| seen.contains(id));
    before - doc.objects.len()
}

/// Renumber objects densely as 1..=n with generation 0 (keeps the existing order).
pub fn renumber(doc: &mut Document) {
    let ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    let map: HashMap<ObjectId, ObjectId> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, (i as u32 + 1, 0)))
        .filter(|(a, b)| a != b)
        .collect();
    if !map.is_empty() {
        remap_document(doc, &map);
        let objects = std::mem::take(&mut doc.objects);
        doc.objects = objects
            .into_iter()
            .map(|(id, o)| (map.get(&id).copied().unwrap_or(id), o))
            .collect();
    }
    doc.max_id = ids.len() as u32;
}

/// Serialize a document the way the final writer does (object + xref streams).
pub fn serialize(doc: &mut Document) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    // Saving allocates ids for object streams; don't let that leak into the document.
    let max_id = doc.max_id;
    let opts = lopdf::SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .max_objects_per_stream(200)
        .compression_level(9)
        .build();
    let r = doc.save_with_options(&mut out, opts);
    doc.max_id = max_id;
    r?;
    Ok(out)
}
