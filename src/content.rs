//! Content-stream walker: collects glyph usage per font and image placements (for effective
//! DPI), following Form XObjects, tiling patterns and annotation appearance streams.

use std::collections::{BTreeSet, HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::util::{decode_stream, num, resolve, resolve_dict};

pub type Matrix = [f64; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// Maximum number of content operators processed in total (protects against pathological
/// nesting of forms). If exceeded, analysis is marked incomplete.
const OP_BUDGET: usize = 50_000_000;
const MAX_DEPTH: usize = 24;

#[derive(Default, Debug)]
pub struct Usage {
    /// Two-byte codes shown with each (indirect) font object.
    pub font_codes: HashMap<ObjectId, BTreeSet<u16>>,
    /// Fonts used in ways we cannot analyse (e.g. via ExtGState /Font).
    pub opaque_fonts: HashSet<ObjectId>,
    /// Largest placed size (width, height) in default user-space points for each image.
    pub image_extent: HashMap<ObjectId, (f64, f64)>,
    /// Objects whose resource references were fully analysed.
    pub walked: HashSet<ObjectId>,
    /// Objects whose content could not be analysed.
    pub failed: HashSet<ObjectId>,
    /// False if the analysis hit a limit; callers must then avoid usage-dependent changes.
    pub complete: bool,
}

impl Usage {
    /// True if every container referencing `id` (from `refs`) was successfully analysed.
    pub fn fully_accounted(&self, refs: Option<&HashSet<ObjectId>>) -> bool {
        match refs {
            None => true,
            Some(r) => r
                .iter()
                .all(|c| self.walked.contains(c) && !self.failed.contains(c)),
        }
    }
}

pub fn mul(m: &Matrix, n: &Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn matrix_of(o: Option<&Object>, doc: &Document) -> Matrix {
    if let Some(Object::Array(a)) = o.map(|o| resolve(doc, o)) {
        if a.len() == 6 {
            let v: Vec<f64> = a.iter().filter_map(|x| num(resolve(doc, x))).collect();
            if v.len() == 6 && v.iter().all(|x| x.is_finite()) {
                return [v[0], v[1], v[2], v[3], v[4], v[5]];
            }
        }
    }
    IDENTITY
}

fn rect_of(o: Option<&Object>, doc: &Document) -> Option<[f64; 4]> {
    if let Object::Array(a) = resolve(doc, o?) {
        let v: Vec<f64> = a.iter().filter_map(|x| num(resolve(doc, x))).collect();
        if v.len() == 4 {
            return Some([
                v[0].min(v[2]),
                v[1].min(v[3]),
                v[0].max(v[2]),
                v[1].max(v[3]),
            ]);
        }
    }
    None
}

/// Resolved resources plus the ids of every indirect object they pass through.
#[derive(Clone, Default)]
struct Res<'a> {
    dict: Option<&'a Dictionary>,
    containers: Vec<ObjectId>,
}

struct Walker<'a> {
    doc: &'a Document,
    usage: Usage,
    ops: usize,
    stack: Vec<ObjectId>,
}

pub fn analyse(doc: &Document) -> Usage {
    let mut w = Walker {
        doc,
        usage: Usage {
            complete: true,
            ..Default::default()
        },
        ops: 0,
        stack: Vec::new(),
    };
    for (_, page_id) in doc.get_pages() {
        w.walk_page(page_id);
    }
    w.usage
}

impl<'a> Walker<'a> {
    fn resources(&self, owner: ObjectId, obj: Option<&'a Object>) -> Res<'a> {
        let mut r = Res {
            dict: None,
            containers: vec![owner],
        };
        let Some(obj) = obj else { return r };
        if let Object::Reference(id) = obj {
            r.containers.push(*id);
        }
        r.dict = resolve_dict(self.doc, obj);
        if let Some(d) = r.dict {
            for (_, v) in d.iter() {
                if let Object::Reference(id) = v {
                    r.containers.push(*id);
                }
            }
        }
        r
    }

    fn walk_page(&mut self, page_id: ObjectId) {
        let doc = self.doc;
        let Ok(page) = doc.get_dictionary(page_id) else {
            return;
        };
        // Find (possibly inherited) resources and the node that owns them.
        let mut node_id = page_id;
        let mut res = Res {
            dict: None,
            containers: vec![page_id],
        };
        for _ in 0..64 {
            let Ok(node) = doc.get_dictionary(node_id) else {
                break;
            };
            if let Ok(r) = node.get(b"Resources") {
                res = self.resources(node_id, Some(r));
                break;
            }
            match node.get(b"Parent") {
                Ok(Object::Reference(p)) => node_id = *p,
                _ => break,
            }
        }
        if !res.containers.contains(&page_id) {
            res.containers.push(page_id);
        }
        let uu = page
            .get(b"UserUnit")
            .ok()
            .and_then(num)
            .filter(|v| *v > 0.0)
            .unwrap_or(1.0);
        let ctm = [uu, 0.0, 0.0, uu, 0.0, 0.0];

        // Concatenate content streams (they form one logical stream).
        let mut data = Vec::new();
        let mut ok = true;
        let mut content_ids = Vec::new();
        match page.get(b"Contents").map(|c| (c, resolve(doc, c))) {
            Ok((_, Object::Array(arr))) => {
                for c in arr {
                    if let Object::Reference(id) = c {
                        content_ids.push(*id);
                    }
                }
            }
            Ok((Object::Reference(id), Object::Stream(_))) => content_ids.push(*id),
            _ => {}
        }
        for id in &content_ids {
            if let Some(Object::Stream(s)) = doc.objects.get(id) {
                match decode_stream(s) {
                    Some(d) => {
                        data.extend_from_slice(&d);
                        data.push(b'\n');
                    }
                    None => ok = false,
                }
            }
        }
        if ok {
            ok = self.walk_stream(&data, &res, ctm, 0);
        }
        self.mark(&res.containers, ok);

        // Annotation appearance streams.
        if let Some(Object::Array(annots)) = page.get(b"Annots").ok().map(|a| resolve(doc, a)) {
            for a in annots {
                let annot_id = if let Object::Reference(id) = a {
                    Some(*id)
                } else {
                    None
                };
                let Some(ad) = resolve_dict(doc, a) else {
                    continue;
                };
                let rect = rect_of(ad.get(b"Rect").ok(), doc);
                let Some(ap) = ad.get(b"AP").ok().and_then(|o| resolve_dict(doc, o)) else {
                    if let Some(id) = annot_id {
                        self.usage.walked.insert(id);
                    }
                    continue;
                };
                let mut all_ok = true;
                for key in [&b"N"[..], b"R", b"D"] {
                    let Ok(entry) = ap.get(key) else { continue };
                    let mut forms = Vec::new();
                    match (entry, resolve(doc, entry)) {
                        (Object::Reference(id), Object::Stream(_)) => forms.push(*id),
                        (_, Object::Dictionary(states)) => {
                            for (_, s) in states.iter() {
                                if let Object::Reference(id) = s {
                                    forms.push(*id);
                                } else {
                                    all_ok = false;
                                }
                            }
                        }
                        _ => all_ok = false,
                    }
                    for fid in forms {
                        let m = self.annot_matrix(fid, rect);
                        all_ok &= self.walk_form(fid, &Res::default(), mul(&m, &ctm), 0);
                    }
                }
                if let Some(id) = annot_id {
                    self.mark(&[id], all_ok);
                }
            }
        }
    }

    /// Matrix mapping an appearance stream to the annotation rectangle (PDF 32000 §12.5.5).
    fn annot_matrix(&self, form_id: ObjectId, rect: Option<[f64; 4]>) -> Matrix {
        let doc = self.doc;
        let Some(Object::Stream(s)) = doc.objects.get(&form_id) else {
            return IDENTITY;
        };
        let m = matrix_of(s.dict.get(b"Matrix").ok(), doc);
        let (Some(bbox), Some(rect)) = (rect_of(s.dict.get(b"BBox").ok(), doc), rect) else {
            return m;
        };
        let pts = [
            (bbox[0], bbox[1]),
            (bbox[2], bbox[1]),
            (bbox[0], bbox[3]),
            (bbox[2], bbox[3]),
        ];
        let tp: Vec<(f64, f64)> = pts
            .iter()
            .map(|(x, y)| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]))
            .collect();
        let minx = tp.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let maxx = tp.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let miny = tp.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let maxy = tp.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        let (bw, bh) = (maxx - minx, maxy - miny);
        if bw <= 0.0 || bh <= 0.0 {
            return m;
        }
        let sx = (rect[2] - rect[0]) / bw;
        let sy = (rect[3] - rect[1]) / bh;

        // Form matrix is applied inside walk_form; return only A here.
        [sx, 0.0, 0.0, sy, rect[0] - minx * sx, rect[1] - miny * sy]
    }

    fn mark(&mut self, ids: &[ObjectId], ok: bool) {
        for id in ids {
            if ok {
                self.usage.walked.insert(*id);
            } else {
                self.usage.failed.insert(*id);
            }
        }
    }

    fn walk_form(
        &mut self,
        form_id: ObjectId,
        parent: &Res<'a>,
        ctm: Matrix,
        depth: usize,
    ) -> bool {
        let doc = self.doc;
        if depth > MAX_DEPTH || self.stack.contains(&form_id) {
            self.usage.complete = false;
            return false;
        }
        let Some(Object::Stream(s)) = doc.objects.get(&form_id) else {
            return false;
        };
        let m = matrix_of(s.dict.get(b"Matrix").ok(), doc);
        let res = match s.dict.get(b"Resources") {
            Ok(r) => self.resources(form_id, Some(r)),
            Err(_) => {
                let mut r = parent.clone();
                r.containers.push(form_id);
                r
            }
        };
        let ok = match decode_stream(s) {
            Some(data) => {
                self.stack.push(form_id);
                let ok = self.walk_stream(&data, &res, mul(&m, &ctm), depth + 1);
                self.stack.pop();
                ok
            }
            None => false,
        };
        self.mark(&res.containers, ok);
        ok
    }

    fn walk_stream(&mut self, data: &[u8], res: &Res<'a>, base: Matrix, depth: usize) -> bool {
        let doc = self.doc;
        let Ok(content) = Content::decode(data) else {
            return false;
        };
        self.ops += content.operations.len();
        if self.ops > OP_BUDGET {
            self.usage.complete = false;
            return false;
        }
        let sub = |key: &[u8]| {
            res.dict
                .and_then(|d| d.get(key).ok())
                .and_then(|o| resolve_dict(doc, o))
        };
        let fonts = sub(b"Font");
        let xobjects = sub(b"XObject");
        let gstates = sub(b"ExtGState");
        let patterns = sub(b"Pattern");

        let mut ok = true;
        let mut ctm = base;
        let mut font: Option<ObjectId> = None;
        let mut stack: Vec<(Matrix, Option<ObjectId>)> = Vec::new();

        for op in &content.operations {
            let o = &op.operands;
            match op.operator.as_str() {
                "q" => stack.push((ctm, font)),
                "Q" => {
                    if let Some((c, f)) = stack.pop() {
                        ctm = c;
                        font = f;
                    }
                }
                "cm" if o.len() == 6 => {
                    let v: Vec<f64> = o.iter().filter_map(num).collect();
                    if v.len() == 6 {
                        ctm = mul(&[v[0], v[1], v[2], v[3], v[4], v[5]], &ctm);
                    }
                }
                "Tf" if !o.is_empty() => {
                    font = None;
                    if let Ok(name) = o[0].as_name() {
                        match fonts.and_then(|f| f.get(name).ok()) {
                            Some(Object::Reference(id)) => font = Some(*id),
                            Some(_) => {}
                            None => ok = false,
                        }
                    }
                }
                "gs" if !o.is_empty() => {
                    if let Ok(name) = o[0].as_name() {
                        if let Some(gd) = gstates
                            .and_then(|g| g.get(name).ok())
                            .and_then(|g| resolve_dict(doc, g))
                        {
                            if let Ok(Object::Array(fa)) = gd.get(b"Font") {
                                if let Some(Object::Reference(id)) = fa.first() {
                                    self.usage.opaque_fonts.insert(*id);
                                    font = Some(*id);
                                }
                            }
                        }
                    }
                }
                "Tj" | "'" | "\"" | "TJ" => {
                    let Some(fid) = font else {
                        continue;
                    };
                    let set = self.usage.font_codes.entry(fid).or_default();
                    let mut add = |b: &[u8]| {
                        for pair in b.chunks_exact(2) {
                            set.insert(u16::from_be_bytes([pair[0], pair[1]]));
                        }
                    };
                    match o.last() {
                        Some(Object::String(b, _)) => add(b),
                        Some(Object::Array(items)) => {
                            for it in items {
                                if let Object::String(b, _) = it {
                                    add(b);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "Do" if !o.is_empty() => {
                    let Ok(name) = o[0].as_name() else { continue };
                    let Some(Object::Reference(xid)) = xobjects.and_then(|x| x.get(name).ok())
                    else {
                        ok = false;
                        continue;
                    };
                    let xid = *xid;
                    let Some(Object::Stream(xs)) = doc.objects.get(&xid) else {
                        continue;
                    };
                    match xs.dict.get(b"Subtype").and_then(|s| s.as_name()) {
                        Ok(b"Image") => {
                            let w = ctm[0].hypot(ctm[1]);
                            let h = ctm[2].hypot(ctm[3]);
                            let e = self.usage.image_extent.entry(xid).or_insert((0.0, 0.0));
                            e.0 = e.0.max(w);
                            e.1 = e.1.max(h);
                        }
                        Ok(b"Form") => {
                            ok &= self.walk_form(xid, res, ctm, depth);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // Tiling patterns referenced by these resources: walk their cells too. Their matrix is
        // relative to the default space of the containing stream.
        if let Some(p) = patterns {
            for (_, v) in p.iter() {
                if let Object::Reference(pid) = v {
                    if let Some(Object::Stream(ps)) = doc.objects.get(pid) {
                        if ps.dict.get(b"PatternType").ok().and_then(num) == Some(1.0) {
                            ok &= self.walk_form(*pid, res, base, depth);
                        }
                    }
                }
            }
        }
        ok
    }
}
