//! TrueType font subsetting for Type0 / CIDFontType2 fonts with Identity encodings
//! (what Microsoft Print to PDF and many other Windows producers embed).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use sha2::{Digest, Sha256};
use subsetter::GlyphRemapper;

use crate::content::Usage;
use crate::util::{
    decode_stream, deflate_best, num, reference_index, resolve, set_flate_compressed,
};

#[derive(Default, Debug)]
pub struct FontStats {
    pub subset: usize,
    pub skipped: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// Where an object lives: indirect (own id) or embedded directly inside a parent object.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Loc {
    Indirect(ObjectId),
    /// Direct object inside the container with this id.
    Direct(ObjectId),
}

impl Loc {
    fn container(self) -> ObjectId {
        match self {
            Loc::Indirect(id) | Loc::Direct(id) => id,
        }
    }
}

struct Candidate {
    type0: ObjectId,
    cid_font: Loc,
    descriptor: Loc,
    font_file: ObjectId,
    /// Old CIDToGIDMap as explicit table, or None for Identity.
    old_map: Option<Vec<u16>>,
    cids: BTreeSet<u16>,
}

fn loc_of(obj: &Object, parent: ObjectId) -> Loc {
    match obj {
        Object::Reference(id) => Loc::Indirect(*id),
        _ => Loc::Direct(parent),
    }
}

fn candidate(doc: &Document, id: ObjectId, usage: &Usage) -> Result<Candidate, &'static str> {
    let d = doc.get_dictionary(id).map_err(|_| "not a dictionary")?;
    if d.get(b"Subtype").and_then(|s| s.as_name()).ok() != Some(b"Type0") {
        return Err("not Type0");
    }
    match resolve(doc, d.get(b"Encoding").map_err(|_| "no encoding")?) {
        Object::Name(n) if n == b"Identity-H" || n == b"Identity-V" => {}
        _ => return Err("non-Identity encoding"),
    }
    let desc_arr = match resolve(doc, d.get(b"DescendantFonts").map_err(|_| "no descendant")?) {
        Object::Array(a) if a.len() == 1 => a,
        _ => return Err("bad DescendantFonts"),
    };
    let desc_container = match d.get(b"DescendantFonts") {
        Ok(Object::Reference(r)) => *r,
        _ => id,
    };
    let cid_font = loc_of(&desc_arr[0], desc_container);
    let cf = match resolve(doc, &desc_arr[0]) {
        Object::Dictionary(cf) => cf,
        _ => return Err("bad CIDFont"),
    };
    if cf.get(b"Subtype").and_then(|s| s.as_name()).ok() != Some(b"CIDFontType2") {
        return Err("not CIDFontType2");
    }
    let fd_obj = cf.get(b"FontDescriptor").map_err(|_| "no descriptor")?;
    let descriptor = loc_of(fd_obj, cid_font.container());
    let fd = match resolve(doc, fd_obj) {
        Object::Dictionary(fd) => fd,
        _ => return Err("bad descriptor"),
    };
    let font_file = match fd.get(b"FontFile2") {
        Ok(Object::Reference(r)) => *r,
        _ => return Err("no FontFile2"),
    };
    let old_map = match cf.get(b"CIDToGIDMap").map(|o| resolve(doc, o)) {
        Err(_) => None,
        Ok(Object::Name(n)) if n == b"Identity" => None,
        Ok(Object::Stream(s)) => {
            let raw = decode_stream(s).ok_or("undecodable CIDToGIDMap")?;
            Some(
                raw.chunks_exact(2)
                    .map(|p| u16::from_be_bytes([p[0], p[1]]))
                    .collect(),
            )
        }
        _ => return Err("bad CIDToGIDMap"),
    };
    if usage.opaque_fonts.contains(&id) {
        return Err("font used via ExtGState");
    }
    let cids = usage.font_codes.get(&id).cloned().unwrap_or_default();
    Ok(Candidate {
        type0: id,
        cid_font,
        descriptor,
        font_file,
        old_map,
        cids,
    })
}

fn get_dict_mut<'a>(doc: &'a mut Document, loc: Loc, path: &[&[u8]]) -> Option<&'a mut Dictionary> {
    // For indirect objects, `path` is ignored. For direct ones, follow `path` from container.
    match loc {
        Loc::Indirect(id) => doc.get_dictionary_mut(id).ok(),
        Loc::Direct(cid) => {
            let mut cur: &mut Object = doc.objects.get_mut(&cid)?;
            for key in path {
                cur = match cur {
                    Object::Dictionary(d) => d.get_mut(key).ok()?,
                    Object::Array(a) => a.get_mut(0)?,
                    _ => return None,
                };
                if let Object::Array(a) = cur {
                    cur = a.get_mut(0)?;
                }
            }
            match cur {
                Object::Dictionary(d) => Some(d),
                _ => None,
            }
        }
    }
}

fn subset_tag(gids: &[u16], name: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(name);
    for g in gids {
        h.update(g.to_be_bytes());
    }
    let digest = h.finalize();
    digest[..6]
        .iter()
        .map(|b| (b'A' + b % 26) as char)
        .collect()
}

fn strip_tag(name: &[u8]) -> &[u8] {
    if name.len() > 7 && name[6] == b'+' && name[..6].iter().all(|c| c.is_ascii_uppercase()) {
        &name[7..]
    } else {
        name
    }
}

fn trim_widths(doc: &Document, w: &Object, used: &BTreeSet<u16>) -> Option<Object> {
    let arr = match resolve(doc, w) {
        Object::Array(a) => a,
        _ => return None,
    };
    let mut widths: BTreeMap<u16, Object> = BTreeMap::new();
    let mut i = 0;
    while i < arr.len() {
        let first = num(resolve(doc, &arr[i]))? as i64;
        match arr.get(i + 1).map(|o| resolve(doc, o)) {
            Some(Object::Array(ws)) => {
                for (k, wv) in ws.iter().enumerate() {
                    let c = first + k as i64;
                    if (0..=u16::MAX as i64).contains(&c) && used.contains(&(c as u16)) {
                        widths.insert(c as u16, resolve(doc, wv).clone());
                    }
                }
                i += 2;
            }
            Some(last) => {
                let last = num(last)? as i64;
                let wv = resolve(doc, arr.get(i + 2)?).clone();
                for c in used.range(first.clamp(0, 65535) as u16..=last.clamp(0, 65535) as u16) {
                    widths.insert(*c, wv.clone());
                }
                i += 3;
            }
            None => return None,
        }
    }
    let mut out = Vec::new();
    let mut run: Vec<Object> = Vec::new();
    let mut start = 0u16;
    let mut prev: Option<u16> = None;
    for (c, w) in widths {
        if prev.map(|p| p as u32 + 1 == c as u32) != Some(true) {
            if !run.is_empty() {
                out.push(Object::Integer(start as i64));
                out.push(Object::Array(std::mem::take(&mut run)));
            }
            start = c;
        }
        run.push(w);
        prev = Some(c);
    }
    if !run.is_empty() {
        out.push(Object::Integer(start as i64));
        out.push(Object::Array(run));
    }
    Some(Object::Array(out))
}

pub fn subset_fonts(doc: &mut Document, usage: &Usage, warn: &mut dyn FnMut(String)) -> FontStats {
    let mut stats = FontStats::default();
    if !usage.complete {
        warn("content analysis incomplete; skipping font subsetting".into());
        return stats;
    }
    let refs = reference_index(doc);
    let type0_ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, o)| {
            o.as_dict()
                .map(|d| d.get(b"Subtype").and_then(|s| s.as_name()).ok() == Some(b"Type0"))
                .unwrap_or(false)
        })
        .map(|(id, _)| *id)
        .collect();

    // Group candidates by font program.
    let mut groups: BTreeMap<ObjectId, Vec<Candidate>> = BTreeMap::new();
    let mut bad_files: HashSet<ObjectId> = HashSet::new();
    for id in type0_ids {
        match candidate(doc, id, usage) {
            Ok(c) => {
                if !usage.fully_accounted(refs.get(&id)) {
                    bad_files.insert(c.font_file);
                    stats.skipped += 1;
                }
                groups.entry(c.font_file).or_default().push(c);
            }
            Err(_) => {
                // Make sure a non-candidate font sharing a font program blocks it.
                if let Ok(d) = doc.get_dictionary(id) {
                    if let Ok(Object::Array(a)) = d.get(b"DescendantFonts").map(|o| resolve(doc, o))
                    {
                        if let Some(Object::Dictionary(cf)) = a.first().map(|o| resolve(doc, o)) {
                            if let Some(Object::Dictionary(fd)) =
                                cf.get(b"FontDescriptor").ok().map(|o| resolve(doc, o))
                            {
                                for k in [&b"FontFile"[..], b"FontFile2", b"FontFile3"] {
                                    if let Ok(Object::Reference(r)) = fd.get(k) {
                                        bad_files.insert(*r);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    for (file_id, cands) in groups {
        if bad_files.contains(&file_id) {
            continue;
        }
        // Every object we will modify must only be referenced from within this group.
        let mut group_containers: HashSet<ObjectId> = HashSet::new();
        for c in &cands {
            group_containers.insert(c.type0);
            group_containers.insert(c.cid_font.container());
            group_containers.insert(c.descriptor.container());
        }
        let owned = |id: ObjectId| {
            refs.get(&id)
                .map(|r| r.iter().all(|c| group_containers.contains(c)))
                .unwrap_or(true)
        };
        let mut ok = owned(file_id);
        for c in &cands {
            for loc in [c.cid_font, c.descriptor] {
                if let Loc::Indirect(i) = loc {
                    ok &= owned(i);
                }
            }
        }
        if !ok {
            stats.skipped += cands.len();
            continue;
        }

        let Some(Object::Stream(ff)) = doc.objects.get(&file_id) else {
            continue;
        };
        let Some(font_data) = decode_stream(ff) else {
            warn(format!(
                "font program {} {} R: cannot decode, skipped",
                file_id.0, file_id.1
            ));
            stats.skipped += cands.len();
            continue;
        };
        let old_encoded = ff.content.len();

        // Glyph set: .notdef plus every glyph reachable from a used CID.
        let gid_for = |c: &Candidate, cid: u16| -> u16 {
            match &c.old_map {
                None => cid,
                Some(m) => m.get(cid as usize).copied().unwrap_or(0),
            }
        };
        let num_glyphs = num_glyphs(&font_data).unwrap_or(0);
        let mut gids: BTreeSet<u16> = BTreeSet::new();
        gids.insert(0);
        for c in &cands {
            for &cid in &c.cids {
                let g = gid_for(c, cid);
                if g < num_glyphs {
                    gids.insert(g);
                }
            }
        }
        let gid_list: Vec<u16> = gids.iter().copied().collect();
        let mut remapper = GlyphRemapper::new();
        for g in &gid_list {
            remapper.remap(*g);
        }
        let result = std::panic::catch_unwind(|| subsetter::subset(&font_data, 0, &remapper));
        let subset = match result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                warn(format!(
                    "font program {} {} R: subsetting failed ({e}), skipped",
                    file_id.0, file_id.1
                ));
                stats.skipped += cands.len();
                continue;
            }
            Err(_) => {
                warn(format!(
                    "font program {} {} R: subsetter panicked, skipped",
                    file_id.0, file_id.1
                ));
                stats.skipped += cands.len();
                continue;
            }
        };
        let compressed = deflate_best(&subset);
        if compressed.len() >= old_encoded {
            stats.skipped += cands.len();
            continue;
        }

        // Font program.
        if let Some(Object::Stream(ff)) = doc.objects.get_mut(&file_id) {
            ff.dict.set("Length1", subset.len() as i64);
            set_flate_compressed(ff, compressed.clone());
        }
        stats.bytes_before += old_encoded;
        stats.bytes_after += compressed.len();

        // Per CIDFont: union of CIDs across all Type0 fonts sharing it.
        // (CIDFont location, a Type0 using it, union of used CIDs, old CID->GID map)
        type CidFontEntry = (Loc, ObjectId, BTreeSet<u16>, Option<Vec<u16>>);
        let mut by_cidfont: Vec<CidFontEntry> = Vec::new();
        for c in &cands {
            if let Some(e) = by_cidfont
                .iter_mut()
                .find(|e| e.0 == c.cid_font && matches!(c.cid_font, Loc::Indirect(_)))
            {
                e.2.extend(c.cids.iter().copied());
            } else {
                by_cidfont.push((c.cid_font, c.type0, c.cids.clone(), c.old_map.clone()));
            }
        }

        let mut done_desc: HashSet<ObjectId> = HashSet::new();
        for (loc, type0, cids, old_map) in by_cidfont {
            let max_cid = cids.iter().next_back().copied().unwrap_or(0) as usize;
            let mut map = vec![0u8; (max_cid + 1) * 2];
            for &cid in &cids {
                let old = match &old_map {
                    None => cid,
                    Some(m) => m.get(cid as usize).copied().unwrap_or(0),
                };
                let new = remapper.get(old).unwrap_or(0);
                map[cid as usize * 2..cid as usize * 2 + 2].copy_from_slice(&new.to_be_bytes());
            }
            let mut map_stream = Stream::new(Dictionary::new(), Vec::new());
            set_flate_compressed(&mut map_stream, deflate_best(&map));
            let map_id = doc.add_object(Object::Stream(map_stream));

            let base_name = doc
                .get_dictionary(type0)
                .ok()
                .and_then(|d| d.get(b"BaseFont").ok())
                .and_then(|n| n.as_name().ok())
                .map(|n| strip_tag(n).to_vec())
                .unwrap_or_else(|| b"Font".to_vec());
            let tag = subset_tag(&gid_list, &base_name);
            let mut new_name = tag.into_bytes();
            new_name.push(b'+');
            new_name.extend_from_slice(&base_name);

            let w_obj = get_dict_mut(doc, loc, &[b"DescendantFonts"])
                .and_then(|cf| cf.get(b"W").ok().cloned());
            let trimmed = w_obj.and_then(|w| trim_widths(doc, &w, &cids));
            let mut fd_ref = None;
            if let Some(cf) = get_dict_mut(doc, loc, &[b"DescendantFonts"]) {
                cf.set("CIDToGIDMap", Object::Reference(map_id));
                cf.set("BaseFont", Object::Name(new_name.clone()));
                if let Some(t) = trimmed {
                    cf.set("W", t);
                }
                match cf.get_mut(b"FontDescriptor") {
                    Ok(Object::Reference(r)) => fd_ref = Some(*r),
                    Ok(Object::Dictionary(fd)) => {
                        fd.set("FontName", Object::Name(new_name.clone()));
                        fd.remove(b"CIDSet");
                    }
                    _ => {}
                }
            }
            // Descriptor: rename and drop the (now wrong) CIDSet.
            if let Some(r) = fd_ref {
                if done_desc.insert(r) {
                    if let Ok(fd) = doc.get_dictionary_mut(r) {
                        fd.set("FontName", Object::Name(new_name.clone()));
                        fd.remove(b"CIDSet");
                    }
                }
            }
            for c in cands.iter().filter(|c| c.cid_font == loc) {
                if let Ok(t0) = doc.get_dictionary_mut(c.type0) {
                    t0.set("BaseFont", Object::Name(new_name.clone()));
                }
            }
            stats.subset += 1;
        }
    }
    stats
}

/// Number of glyphs from the `maxp` table of a TrueType/OpenType font.
fn num_glyphs(data: &[u8]) -> Option<u16> {
    let rd16 = |o: usize| data.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let rd32 = |o: usize| {
        data.get(o..o + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let n = rd16(4)? as usize;
    for i in 0..n {
        let rec = 12 + i * 16;
        if data.get(rec..rec + 4)? == b"maxp" {
            let off = rd32(rec + 8)? as usize;
            return rd16(off + 4);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_compacts_widths() {
        let doc = Document::new();
        let w = Object::Array(vec![
            1.into(),
            Object::Array(vec![100.into(), 200.into(), 300.into()]),
            10.into(),
            20.into(),
            500.into(),
        ]);
        let used: BTreeSet<u16> = [2, 3, 12, 13, 40].into_iter().collect();
        let t = trim_widths(&doc, &w, &used).unwrap();
        let expect = Object::Array(vec![
            2.into(),
            Object::Array(vec![200.into(), 300.into()]),
            12.into(),
            Object::Array(vec![500.into(), 500.into()]),
        ]);
        assert_eq!(format!("{t:?}"), format!("{expect:?}"));
    }

    #[test]
    fn subset_tags() {
        assert_eq!(strip_tag(b"ABCDEF+Foo"), b"Foo");
        assert_eq!(strip_tag(b"CIDFont+F1"), b"CIDFont+F1");
        let t = subset_tag(&[0, 5, 9], b"Foo");
        assert_eq!(t.len(), 6);
        assert!(t.bytes().all(|c| c.is_ascii_uppercase()));
    }
}
