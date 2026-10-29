//! Structural optimizations: Flate recompression of every stream and removal of unused objects.
//! Object streams and cross-reference streams are produced by the writer (see `util::serialize`).

use lopdf::{Document, Object};

use crate::util::{
    decode_stream, deflate_best, filters, is_fully_decodable, prune_unreferenced, renumber,
    set_flate_compressed,
};

#[derive(Default, Debug)]
pub struct StructureStats {
    pub recompressed: usize,
    pub pruned: usize,
}

pub fn optimize_structure(doc: &mut Document) -> StructureStats {
    let mut stats = StructureStats {
        pruned: prune_unreferenced(doc),
        ..Default::default()
    };
    for obj in doc.objects.values_mut() {
        let Object::Stream(s) = obj else { continue };
        match s.dict.get_type().ok() {
            // XMP metadata is conventionally left uncompressed so it remains discoverable.
            Some(b"Metadata") | Some(b"XRef") | Some(b"ObjStm") => continue,
            _ => {}
        }
        if !is_fully_decodable(s) {
            continue;
        }
        let Some(raw) = decode_stream(s) else {
            continue;
        };
        let packed = deflate_best(&raw);
        let only_flate = filters(s)
            .map(|f| f.len() == 1 && f[0] == b"FlateDecode")
            .unwrap_or(false);
        // Keep a margin for the added /Filter entry when compressing an unfiltered stream.
        let margin = if filters(s).map(|f| f.is_empty()).unwrap_or(false) {
            20
        } else {
            0
        };
        if packed.len() + margin < s.content.len()
            || (!only_flate && packed.len() <= s.content.len())
        {
            set_flate_compressed(s, packed);
            stats.recompressed += 1;
        }
    }
    renumber(doc);
    stats
}
