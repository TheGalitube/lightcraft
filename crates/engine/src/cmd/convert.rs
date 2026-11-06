//! Convert to DNG: a raw photo's original is re-encoded as a lossless DNG (its develop settings
//! embedded as XMP) next to it, and the photo is relinked to the DNG. The original stays on
//! disk; undo relinks the photo to it.

use std::path::Path;

use lightcraft_catalog::{MediaKind, Op, Source};
use serde_json::{Value, json};

use super::{CommandSpec, bad, cmd, has_active, has_selection};
use crate::{Result, Session};

/// `<stem>.dng`, then `<stem>-2.dng`, `<stem>-3.dng`… next to `path`.
fn dng_names(path: &str) -> impl Iterator<Item = std::path::PathBuf> {
    let p = Path::new(path);
    let dir = p.parent().unwrap_or(Path::new("")).to_path_buf();
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "photo".into());
    (1u64..).map(move |n| if n == 1 { dir.join(format!("{stem}.dng")) } else { dir.join(format!("{stem}-{n}.dng")) })
}

/// Write the DNG for raw file `path` (develop settings in `packet`) next to it under a free name;
/// returns the new path. An empty `packet` writes no XMP.
///
/// The DNG is checked before anyone relies on it (Convert to DNG relinks the photo to it, Copy as
/// DNG deletes the raw copy): the encoded file must decode to the same raw data, and it is
/// written to a temp file, synced, read back identical and only then given its name — never
/// replacing an existing file. On any failure no DNG is left behind and the raw stays in use.
pub(crate) fn write_dng_for(s: &Session, path: &str, packet: String) -> std::result::Result<String, String> {
    write_dng_with(s.media.file_bytes.as_ref(), path, packet)
}

/// [`write_dng_for`] without the session (imports convert on a worker thread).
pub(crate) fn write_dng_with(file_bytes: Option<&crate::merge::ByteReader>, path: &str, packet: String) -> std::result::Result<String, String> {
    let bytes = match file_bytes {
        Some(r) => r(path)?,
        None => std::fs::read(path).map_err(|e| format!("{path}: {e}"))?,
    };
    let mut raw = lightcraft_raw::decode(&bytes).map_err(|e| format!("{path}: {e}"))?;
    crate::camera_preview::with_starting_colour(&mut raw, &bytes);
    drop(bytes);
    let dng = lightcraft_raw::write_dng(&raw, &lightcraft_raw::DngWriteOptions { xmp: (!packet.is_empty()).then_some(packet), ..Default::default() })
        .map_err(|e| e.to_string())?;
    let back = lightcraft_raw::decode(&dng).map_err(|e| format!("{path}: the DNG written for it doesn't decode ({e}); the raw is kept"))?;
    let same = match (&back.data, &raw.data) {
        (lightcraft_raw::RawData::U16(a), lightcraft_raw::RawData::U16(b)) => a == b,
        (lightcraft_raw::RawData::F32(a), lightcraft_raw::RawData::F32(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
        }
        _ => false,
    };
    if (back.width, back.height, back.cpp) != (raw.width, raw.height, raw.cpp) || !same {
        return Err(format!("{path}: the DNG written for it doesn't hold the same raw data; the raw is kept"));
    }
    drop((raw, back));
    let out = lightcraft_catalog::safe_file::write_new_unique(&mut dng_names(path), &dng)
        .map_err(|e| format!("{path}: could not write its DNG ({e}); the raw is kept"))?;
    Ok(out.to_string_lossy().to_string())
}

fn convert(s: &mut Session, p: &Value) -> Result<Value> {
    convert_with_size(s, p, |path| std::fs::metadata(path).map(|metadata| metadata.len()))
}

/// Convert each target independently; the size reader allows deterministic filesystem-failure tests
/// after the real DNG write, without process-wide fault injection or timing-dependent races.
fn convert_with_size(s: &mut Session, p: &Value, read_size: impl Fn(&str) -> std::io::Result<u64>) -> Result<Value> {
    const C: &str = "photo.convertToDng";
    let ids = s.targets(p)?;
    let mut ops = Vec::new();
    let mut converted = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        let Some(ph) = s.catalog.photo(id).cloned() else { continue };
        let Source::File { path } = &ph.source else {
            skipped.push(json!([id.0, "not a file"]));
            continue;
        };
        if ph.kind != MediaKind::Raw || ph.format.eq_ignore_ascii_case("DNG") || ph.copy_of.is_some() {
            skipped.push(json!([
                id.0,
                if ph.kind != MediaKind::Raw {
                    "not a raw photo"
                } else if ph.copy_of.is_some() {
                    "a virtual copy"
                } else {
                    "already a DNG"
                }
            ]));
            continue;
        }
        let packet = crate::sidecar::sidecar_packet(&ph, &s.catalog);
        match write_dng_for(s, path, packet) {
            Ok(out) => {
                let file_name = Path::new(&out).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let file_size = match read_size(&out) {
                    Ok(size) => size,
                    Err(error) => {
                        let mut reason = format!("{out}: could not read converted file size ({error}); the raw is kept");
                        if let Err(cleanup) = std::fs::remove_file(&out)
                            && cleanup.kind() != std::io::ErrorKind::NotFound
                        {
                            reason.push_str(&format!("; could not remove unused DNG ({cleanup})"));
                        }
                        skipped.push(json!([id.0, reason]));
                        continue;
                    }
                };
                // The original and its virtual copies share the new file, but keep their own metadata.
                for c in s.catalog.photos().filter(|c| c.copy_of == Some(id)).chain(std::iter::once(&ph)) {
                    ops.push(Op::Relink {
                        id: c.id,
                        file_name: file_name.clone(),
                        source: Source::File { path: out.clone() },
                        format: Some("DNG".into()),
                    });
                    ops.push(Op::SetContent {
                        id: c.id,
                        width: c.width,
                        height: c.height,
                        file_size,
                        content_hash: c.content_hash.clone(),
                        preview_only: c.preview_only.clone(),
                        preview_checked: c.preview_checked.clone(),
                    });
                }
                converted.push(json!({"id": id.0, "path": out, "original": path}));
            }
            Err(e) => skipped.push(json!([id.0, e])),
        }
    }
    if !ops.is_empty() {
        let n = converted.len();
        s.commit(&format!("Convert {n} Photo{} to DNG", if n == 1 { "" } else { "s" }), Op::Batch { ops }).map_err(|e| bad(C, e.to_string()))?;
    }
    Ok(json!({"converted": converted, "skipped": skipped}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "photo.convertToDng",
        "Convert to DNG",
        ["Photo"],
        None,
        "{ids?} — write each raw photo as a lossless DNG next to it (settings embedded) and relink the photo to it; originals are kept → {converted: [{id, path, original}], skipped}",
        has_selection,
        convert
    )]
}

/// Edit in an external editor, the engine half: render the photo with its edits as a 16-bit
/// TIFF (or PSD, or 8-bit) next to the original (`<name>-Edit.tif`, never overwriting), add it to
/// the library and stack it on top of the original. The host opens the file in the editor.
fn edit_external(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "photo.editExternal";
    let mut ids = s.targets(p)?;
    ids.dedup();
    if ids.is_empty() {
        return Err(bad(C, "no active photo"));
    }
    if ids.len() > MAX_EDIT_COPIES {
        return Err(bad(C, format!("{} photos selected: edit at most {MAX_EDIT_COPIES} at a time", ids.len())));
    }
    let space = match super::str_param(p, "colorSpace").unwrap_or("adobeRgb") {
        "srgb" => lightcraft_pipeline::OutputSpace::Srgb,
        "displayP3" => lightcraft_pipeline::OutputSpace::DisplayP3,
        "prophoto" | "proPhoto" => lightcraft_pipeline::OutputSpace::ProPhoto,
        _ => lightcraft_pipeline::OutputSpace::AdobeRgb,
    };
    let format = match super::str_param(p, "format").unwrap_or("tiff") {
        "tiff" | "tif" => crate::export::ExportFormat::Tiff,
        "psd" => crate::export::ExportFormat::Psd,
        f => return Err(bad(C, format!("format {f:?}: use tiff or psd"))),
    };
    let bit_depth = match p.get("bitDepth").and_then(Value::as_u64).unwrap_or(16) {
        8 => 8,
        16 => 16,
        d => return Err(bad(C, format!("bitDepth {d}: use 8 or 16"))),
    };
    let opts = crate::export::ExportOptions {
        format,
        bit_depth: Some(bit_depth),
        color_space: space,
        naming: "{name}-Edit".into(),
        conflict: crate::export::Conflict::Unique,
        ..Default::default()
    };
    let mut copies = Vec::new();
    for id in ids {
        copies.push(edit_copy(s, p, id, &opts)?);
    }
    let new: Vec<lightcraft_catalog::PhotoId> = copies.iter().filter_map(|c| c["id"].as_u64()).map(lightcraft_catalog::PhotoId).collect();
    s.selection = crate::Selection { active: new.first().copied(), ids: new };
    let first = copies.first().cloned().unwrap_or(Value::Null);
    Ok(json!({"path": first["path"], "id": first["id"], "original": first["original"], "copies": copies}))
}

/// Most photos one Edit In renders (each becomes a full-size file on disk and a library photo).
const MAX_EDIT_COPIES: usize = 50;

/// One photo's edit copy for [`edit_external`]: rendered with `opts` next to its original (a demo
/// photo into `dir`), added to the library and stacked on top of the original, expanded.
fn edit_copy(s: &mut Session, p: &Value, id: lightcraft_catalog::PhotoId, opts: &crate::export::ExportOptions) -> Result<Value> {
    const C: &str = "photo.editExternal";
    let ph = s.catalog.photo(id).cloned().ok_or_else(|| bad(C, format!("no photo {}", id.0)))?;
    let dir = match &ph.source {
        Source::File { path } => Path::new(path).parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(),
        Source::Demo { .. } => match super::str_param(p, "dir") {
            Some(d) => d.to_string(),
            None => return Err(bad(C, "a generated demo photo has no folder: give `dir`")),
        },
    };
    let mut written = Vec::new();
    let mut write = |path: &str, bytes: &[u8]| -> std::result::Result<(), String> {
        // durable: the TIFF becomes a library photo the external editor then changes
        crate::export::write_file_durable(path, bytes)?;
        written.push(path.to_string());
        Ok(())
    };
    let to = crate::export::Destination { dir, exact: None };
    crate::export::export_batch(s, &[id], opts, &to, &mut write, &|path| Path::new(path).exists()).map_err(|e| bad(C, e))?;
    let ext = format!(".{}", opts.format.extension());
    let out = written.into_iter().find(|w| w.ends_with(&ext)).ok_or_else(|| bad(C, "nothing was written"))?;
    let r = s.execute("library.import", &json!({"paths": [out]}))?;
    let new = r["imported"].get(0).and_then(Value::as_u64).ok_or_else(|| bad(C, "the edit copy could not be added"))?;
    // stack: the edit on top of the original, expanded so both show
    let _ = s.execute("stack.group", &json!({"ids": [new, id.0], "top": new, "collapsed": false}));
    Ok(json!({"path": out, "id": new, "original": id.0}))
}

/// The op that brings a photo up to date with what its file is now (`info`, a fresh probe), or
/// `None` when nothing changed. Covers a raw that became decodable (or stopped being: a different
/// file relinked), i.e. a change of [`lightcraft_catalog::Photo::preview_only`], and a
/// preview-only photo this version hasn't checked yet: its stamp is refreshed so it isn't probed
/// again (see [`lightcraft_catalog::Photo::preview_checked`]).
pub(crate) fn content_op(id: lightcraft_catalog::PhotoId, ph: &lightcraft_catalog::Photo, info: crate::media::ProbeInfo) -> Option<Op> {
    let preview_checked = lightcraft_catalog::preview_stamp(&info.preview_only);
    let same = info.content_hash == ph.content_hash
        && info.file_size == ph.file_size
        && (info.width, info.height) == (ph.width, ph.height)
        && info.preview_only == ph.preview_only
        && preview_checked == ph.preview_checked;
    (!same).then_some(Op::SetContent {
        id,
        width: info.width,
        height: info.height,
        file_size: info.file_size,
        content_hash: info.content_hash,
        preview_only: info.preview_only,
        preview_checked,
    })
}

/// Ops that fill a photo's empty camera fields (camera, lens, exposure, GPS, capture time) from a fresh probe.
fn fill_missing_meta(id: lightcraft_catalog::PhotoId, ph: &lightcraft_catalog::Photo, info: &crate::media::ProbeInfo) -> Vec<Op> {
    let (mut m, src) = (ph.meta.clone(), &info.meta);
    let before = m.clone();
    if m.camera.is_empty() {
        m.camera = src.camera.clone();
    }
    if m.lens.is_empty() {
        m.lens = src.lens.clone();
    }
    if m.shutter.is_empty() {
        m.shutter = src.shutter.clone();
    }
    m.focal_mm = m.focal_mm.or(src.focal_mm);
    m.aperture = m.aperture.or(src.aperture);
    m.iso = m.iso.or(src.iso);
    m.gps = m.gps.or(src.gps);
    // the camera's focus point: photos imported before it was read get it here
    if m.focus.is_none() {
        m.focus = src.focus.clone();
    }
    let mut ops = Vec::new();
    if m != before {
        ops.push(Op::SetMeta { id, meta: Box::new(m) });
    }
    if ph.captured.is_none() && info.captured.is_some() {
        ops.push(Op::SetCaptured { id, captured: info.captured.clone() });
    }
    ops
}

/// Ops that give a photo the lens data its file carries now (`info`, a fresh probe), or none when
/// it already has them. A catalog from before the lens data was read at import has none, so such a
/// photo's correction stayed off. When the photo gains lens data and its develop settings are still
/// what import gave it, the lens correction is turned on as import would; an edited photo keeps
/// its settings.
pub(crate) fn lens_ops(id: lightcraft_catalog::PhotoId, ph: &lightcraft_catalog::Photo, info: &crate::media::ProbeInfo) -> Vec<Op> {
    if info.embedded_lens == ph.embedded_lens {
        return Vec::new();
    }
    let mut ops = vec![Op::SetEmbeddedLens { id, lens: info.embedded_lens.map(Box::new) }];
    // (a photo with a default-preset look keeps it: that look is its import state)
    if ph.embedded_lens.is_none() && info.embedded_lens.is_some() && ph.import_look.is_none() && !ph.is_edited() && !ph.develop.optics.lens_profile {
        let mut d = (*ph.develop).clone();
        d.optics.lens_profile = true;
        ops.push(Op::SetDevelop { id, settings: std::sync::Arc::new(d), label: "Lens Corrections".into(), edited: ph.edited.clone() });
    }
    ops
}

/// The op that gives a raw the as-shot white balance its file reads now (`info`, a fresh probe), or none when the
/// catalog already has it or the photo is not a raw edited in Kelvin. A raw imported before its camera's matrices
/// gave it a Kelvin scale keeps the relative scale's 6500 / 0 until a decode or Reload reads the file (issue #730).
pub(crate) fn as_shot_wb_op(id: lightcraft_catalog::PhotoId, ph: &lightcraft_catalog::Photo, info: &crate::media::ProbeInfo) -> Option<Op> {
    let read = info.as_shot_wb.filter(|(t, tint)| t.is_finite() && tint.is_finite())?;
    (ph.develops_raw() && !ph.relative_wb() && ph.as_shot_wb != Some(read)).then_some(Op::SetAsShotWb { id, wb: Some(read) })
}

/// The ops that bring photo `id` and its virtual copies up to date with what their file is now
/// (`info`, a fresh probe), appended to `ops`; the photos they change are appended to `changed`.
/// Reload and the preview-only recheck (`library.recheckPreviewOnly`) share it.
pub(crate) fn reload_ops(
    cat: &lightcraft_catalog::Catalog,
    id: lightcraft_catalog::PhotoId,
    info: &crate::media::ProbeInfo,
    ops: &mut Vec<Op>,
    changed: &mut Vec<lightcraft_catalog::PhotoId>,
) {
    let Some(ph) = cat.photo(id) else { return };
    // camera fields the catalog lacks (e.g. a raw imported before its format was read) are filled in;
    // nothing already set is overwritten
    let meta_ops = fill_missing_meta(id, ph, info);
    let mut mine: Vec<Op> =
        content_op(id, ph, info.clone()).into_iter().chain(meta_ops).chain(lens_ops(id, ph, info)).chain(as_shot_wb_op(id, ph, info)).collect();
    if !mine.is_empty() {
        ops.append(&mut mine);
        changed.push(id);
    }
    // virtual copies share the file
    for c in cat.photos().filter(|c| c.copy_of == Some(id)) {
        let theirs: Vec<Op> =
            content_op(c.id, c, info.clone()).into_iter().chain(lens_ops(c.id, c, info)).chain(as_shot_wb_op(c.id, c, info)).collect();
        if !theirs.is_empty() {
            ops.extend(theirs);
            changed.push(c.id);
        }
    }
}

/// Re-read photos whose files changed on disk (an external editor saved them): new size,
/// dimensions and content hash, cached sources dropped. The app runs the same steps
/// ([`crate::cmd::reload`]) with the files read on a worker thread. → {reloaded: [ids]}
fn reload(s: &mut Session, p: &Value) -> Result<Value> {
    use crate::cmd::reload as r;
    let items = r::plan(s, p)?;
    let probe = r::file_probe(s);
    let results = r::probe(probe.as_ref(), &items, &std::sync::atomic::AtomicBool::new(false), &|_| {});
    let reloaded = r::apply(s, items, results)?;
    Ok(r::to_json(&reloaded))
}

/// Duplicate: copy the file next to itself (`<name>-copy`, never overwriting) and add it with the
/// same settings, metadata, rating, flag, label and albums. → {ids}
fn duplicate(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "photo.duplicate";
    let mut made = Vec::new();
    for id in s.targets(p)? {
        let Some(ph) = s.catalog.photo(id).map(|p| (**p).clone()) else { continue };
        let Source::File { path } = &ph.source else { return Err(bad(C, "a generated demo photo has no file to duplicate")) };
        let src = Path::new(path);
        let stem = src.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| "photo".into());
        let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let dir = src.parent().unwrap_or(Path::new(""));
        let mut dst = dir.join(format!("{stem}-copy{ext}"));
        let mut n = 2;
        while dst.exists() {
            dst = dir.join(format!("{stem}-copy-{n}{ext}"));
            n += 1;
        }
        std::fs::copy(src, &dst).map_err(|e| bad(C, format!("{path}: {e}")))?;
        let new = s.catalog.alloc_photo_id();
        let mut q = ph.clone();
        q.id = new;
        q.source = Source::File { path: dst.to_string_lossy().to_string() };
        q.file_name = dst.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        // the same bytes, but its own photo: no duplicate-content clash, no shared history
        q.content_hash = q.content_hash.map(|h| format!("{h}:dup{}", new.0));
        q.copy_of = None;
        q.copy_name = None;
        q.history.clear();
        q.versions.retain(|v| !v.auto);
        let mut ops = vec![Op::AddPhoto { photo: Box::new(q) }];
        for a in s.catalog.albums().filter(|a| !a.is_smart() && !a.folder && a.photos.contains(&id)) {
            let mut photos = a.photos.clone();
            photos.push(new);
            ops.push(Op::SetAlbumPhotos { id: a.id, photos });
        }
        s.commit("Duplicate", Op::Batch { ops })?;
        made.push(new);
    }
    s.merge_undo(made.len(), "Duplicate");
    if let Some(last) = made.last() {
        s.selection = crate::Selection { ids: made.clone(), active: Some(*last) };
    }
    Ok(json!({"ids": made.iter().map(|i| i.0).collect::<Vec<_>>()}))
}

pub fn edit_specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "photo.duplicate",
            "Duplicate",
            ["Photo"],
            None,
            "{ids?} — copy each photo's file next to it (`-copy`) and add it with the same settings, metadata and albums (a real file, unlike a virtual copy) → {ids}",
            has_selection,
            duplicate
        ),
        cmd!(
            "photo.reload",
            "Reload from Disk",
            ["Photo"],
            None,
            "{ids?} — re-read photos whose files changed on disk (e.g. saved by an external editor), or raws shown from their embedded preview that can be decoded now; camera fields the catalog lacks are filled in, and so is the lens data the file carries (an untouched photo then gets its lens correction turned on as import would; an edited one keeps its settings); in the app the files are read in the background (activity row `reload`, cancellable) and the reply is {background: true, count} unless `wait: true` → {reloaded}",
            has_selection,
            reload
        ),
        cmd!(
            "photo.editExternal",
            "Edit Copy for External Editor",
            [],
            None,
            "{ids?, colorSpace?: adobeRgb (default) | proPhoto | displayP3 | srgb, format?: tiff (default) | psd, bitDepth?: 16 (default) | 8, dir?} — render each selected photo (or `ids`, at most 50) with its edits as `<name>-Edit.tif` (or `.psd`, flat, which PhotoCraft saves back to in place) next to it, add each stacked on its original and select the copies → {path, id, original (the first copy), copies: [{path, id, original}]} (the app then opens them in the external editor)",
            has_active,
            edit_external
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_catalog::{Photo, PhotoId, Source};

    /// Three real synthetic raw files and a virtual copy of each, in an on-disk library.
    fn conversion_fixture(tag: &str) -> (std::path::PathBuf, Session, Vec<(PhotoId, PhotoId)>) {
        let dir = crate::tests::xmp::temp_dir(tag);
        let mut s = Session::new().with_fs();
        s.open_library(dir.join("library"), false).unwrap();
        let mut pairs = Vec::new();
        for name in ["before", "Größe mit Leerzeichen", "after"] {
            let path = dir.join(format!("{name}.nef"));
            let mut bytes = crate::tests::xmp::synthetic_dng_with(None, Default::default());
            bytes.extend_from_slice(&[0; 8192]); // A size difference independent of embedded XMP length.
            std::fs::write(&path, &bytes).unwrap();
            let id = s.catalog.alloc_photo_id();
            let mut photo = Photo::new(id, Source::File { path: path.to_string_lossy().into_owned() }, &format!("{name}.nef"), "NEF", 32, 24, "");
            photo.kind = MediaKind::Raw;
            photo.file_size = bytes.len() as u64;
            s.commit("Fixture", Op::AddPhoto { photo: Box::new(photo) }).unwrap();
            s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
            let copy = PhotoId(s.execute("photo.virtualCopy", &json!({})).unwrap()["ids"][0].as_u64().unwrap());
            s.execute("library.select", &json!({"ids": [copy.0]})).unwrap();
            s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.75})).unwrap();
            pairs.push((id, copy));
        }
        (dir, s, pairs)
    }

    /// A post-write size failure skips only its photo, cleans its output, and preserves successful
    /// conversions on both sides of it, including virtual copies, history and library persistence.
    #[test]
    fn convert_to_dng_size_failure_preserves_other_photos() {
        let (dir, mut s, pairs) = conversion_fixture("dng-size-failure");
        let before: Vec<_> = pairs.iter().flat_map(|(id, copy)| [*id, *copy]).map(|id| s.catalog.photo(id).unwrap().clone()).collect();
        let ids: Vec<_> = pairs.iter().map(|(id, _)| id.0).collect();
        let failed_out = dir.join("Größe mit Leerzeichen.dng");
        let undo_len = s.undo.len();
        let r = convert_with_size(&mut s, &json!({"ids": ids}), |path| {
            assert!(Path::new(path).is_file(), "failure happens after the real DNG write");
            if Path::new(path) == failed_out {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected metadata failure"))
            } else {
                std::fs::metadata(path).map(|m| m.len())
            }
        })
        .unwrap();
        assert_eq!(r["converted"].as_array().unwrap().len(), 2, "{r}");
        assert_eq!(r["skipped"].as_array().unwrap().len(), 1, "{r}");
        assert_eq!(r["skipped"][0][0], pairs[1].0.0);
        assert!(r["skipped"][0][1].as_str().unwrap().contains("injected metadata failure"));
        assert!(!failed_out.exists(), "no orphaned DNG");
        assert_eq!(s.undo.len(), undo_len + 1, "one step for all successful conversions");
        for old in &before {
            let now = s.catalog.photo(old.id).unwrap();
            let Source::File { path } = &old.source else { panic!("file fixture") };
            assert!(Path::new(path).is_file(), "original kept");
            if old.id == pairs[1].0 || old.id == pairs[1].1 {
                assert_eq!(serde_json::to_value(now).unwrap(), serde_json::to_value(old).unwrap(), "failed photo and copy unchanged");
            } else {
                let out = Path::new(path).with_extension("dng");
                let mut expected = (**old).clone();
                expected.source = Source::File { path: out.to_string_lossy().into_owned() };
                expected.file_name = out.file_name().unwrap().to_string_lossy().into_owned();
                expected.format = "DNG".into();
                expected.file_size = std::fs::metadata(&out).unwrap().len();
                assert_ne!(expected.file_size, old.file_size);
                assert_eq!(serde_json::to_value(now).unwrap(), serde_json::to_value(expected).unwrap(), "only source, format, name and size change");
            }
        }
        let converted: Vec<_> = before.iter().map(|ph| s.catalog.photo(ph.id).unwrap().clone()).collect();
        s.execute("edit.undo", &json!({})).unwrap();
        for old in &before {
            assert_eq!(serde_json::to_value(s.catalog.photo(old.id).unwrap()).unwrap(), serde_json::to_value(old).unwrap());
        }
        s.execute("edit.redo", &json!({})).unwrap();
        s.persist().unwrap();
        drop(s);
        let mut reopened = Session::new().with_fs();
        reopened.open_library(dir.join("library"), false).unwrap();
        for expected in &converted {
            assert_eq!(serde_json::to_value(reopened.catalog.photo(expected.id).unwrap()).unwrap(), serde_json::to_value(expected).unwrap());
        }
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Saving after Undo must persist the original sizes and paths for originals and virtual copies.
    #[test]
    fn convert_to_dng_undone_size_survives_reopen() {
        let (dir, mut s, pairs) = conversion_fixture("dng-undone-reopen");
        let before: Vec<_> = s.catalog.photos().cloned().collect();
        let ids: Vec<_> = pairs.iter().map(|(id, _)| id.0).collect();
        let r = s.execute("photo.convertToDng", &json!({"ids": ids})).unwrap();
        assert_eq!(r["converted"].as_array().unwrap().len(), 3);
        assert_eq!(r["skipped"], json!([]));
        s.execute("edit.undo", &json!({})).unwrap();
        s.execute("edit.redo", &json!({})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        s.persist().unwrap();
        drop(s);
        let mut reopened = Session::new().with_fs();
        reopened.open_library(dir.join("library"), false).unwrap();
        for expected in &before {
            assert_eq!(serde_json::to_value(reopened.catalog.photo(expected.id).unwrap()).unwrap(), serde_json::to_value(expected).unwrap());
        }
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// If every size lookup fails, the catalog and undo history remain unchanged and no DNG remains.
    #[test]
    fn convert_to_dng_all_size_failures_leave_no_step() {
        let (dir, mut s, pairs) = conversion_fixture("dng-all-size-failures");
        let before: Vec<_> = s.catalog.photos().map(|ph| serde_json::to_value(ph).unwrap()).collect();
        let undo_len = s.undo.len();
        let ids: Vec<_> = pairs.iter().map(|(id, _)| id.0).collect();
        let r = convert_with_size(&mut s, &json!({"ids": ids}), |_| Err(std::io::Error::other("injected metadata failure"))).unwrap();
        assert_eq!(r["converted"], json!([]));
        assert_eq!(r["skipped"].as_array().unwrap().len(), 3);
        assert_eq!(s.undo.len(), undo_len);
        assert_eq!(s.catalog.photos().map(|ph| serde_json::to_value(ph).unwrap()).collect::<Vec<_>>(), before);
        assert!(!std::fs::read_dir(&dir).unwrap().any(|e| e.unwrap().path().extension().is_some_and(|ext| ext == "dng")));
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file already removed by another actor needs no cleanup; other cleanup errors are reported
    /// alongside the metadata failure instead of aborting the remaining conversions.
    #[test]
    fn convert_to_dng_size_failure_reports_cleanup_errors() {
        for replace_with_directory in [false, true] {
            let (dir, mut s, pairs) = conversion_fixture(if replace_with_directory { "dng-cleanup-error" } else { "dng-already-gone" });
            let failed_out = dir.join("before.dng");
            let original = s.catalog.photo(pairs[0].0).unwrap().clone();
            let r = convert_with_size(&mut s, &json!({"ids": [pairs[0].0.0, pairs[2].0.0]}), |path| {
                if Path::new(path) == failed_out {
                    std::fs::remove_file(path).unwrap();
                    if replace_with_directory {
                        std::fs::create_dir(path).unwrap();
                    }
                    Err(std::io::Error::other("injected metadata failure"))
                } else {
                    std::fs::metadata(path).map(|m| m.len())
                }
            })
            .unwrap();
            assert_eq!(r["converted"].as_array().unwrap().len(), 1);
            assert_eq!(r["skipped"].as_array().unwrap().len(), 1);
            let reason = r["skipped"][0][1].as_str().unwrap();
            assert!(reason.contains("injected metadata failure"));
            assert_eq!(reason.contains("could not remove unused DNG"), replace_with_directory);
            assert_eq!(serde_json::to_value(s.catalog.photo(original.id).unwrap()).unwrap(), serde_json::to_value(original).unwrap());
            drop(s);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Reload fills camera fields a photo lacks (a CR3 imported before CR3 metadata was read) and keeps
    /// everything already set.
    #[test]
    fn reload_fills_only_missing_camera_fields() {
        let mut ph = Photo::new(PhotoId(1), Source::File { path: "/x.cr3".into() }, "x.cr3", "CR3", 1620, 1080, "2026-10-06T00:00:00");
        ph.meta.title = "Mine".into();
        ph.meta.iso = Some(800);
        let mut info = crate::media::ProbeInfo { captured: Some("2026-10-04T08:16:11".into()), ..Default::default() };
        info.meta.camera = "Canon EOS R6 Mark III".into();
        info.meta.lens = "RF24-105mm".into();
        info.meta.iso = Some(400);
        info.meta.aperture = Some(6.3);
        let ops = fill_missing_meta(PhotoId(1), &ph, &info);
        let Some(Op::SetMeta { meta, .. }) = ops.first() else { panic!("{ops:?}") };
        assert_eq!((meta.camera.as_str(), meta.lens.as_str(), meta.aperture), ("Canon EOS R6 Mark III", "RF24-105mm", Some(6.3)));
        assert_eq!((meta.title.as_str(), meta.iso), ("Mine", Some(800)), "set fields are kept");
        assert!(matches!(ops.get(1), Some(Op::SetCaptured { captured: Some(_), .. })));
        // nothing missing → nothing to do
        ph.meta = (**meta).clone();
        ph.captured = Some("2026-10-04T08:16:11".into());
        assert!(fill_missing_meta(PhotoId(1), &ph, &info).is_empty());
    }

    /// Reload from Disk gives a photo imported before focus points were read its camera focus point,
    /// and never replaces one it has.
    #[test]
    fn reload_fills_a_missing_focus_point() {
        let fp = |x: f64| lightcraft_meta::FocusPoint {
            rect: lightcraft_meta::Rect { x0: x, y0: 0.4, x1: x + 0.02, y1: 0.42 },
            mode: Some("AF-C".into()),
            area: None,
            tracking: None,
        };
        let mut ph = Photo::new(PhotoId(1), Source::File { path: "/x.arw".into() }, "x.arw", "ARW", 8640, 5760, "2026-10-06T00:00:00");
        ph.captured = Some("2026-10-04T08:16:11".into());
        let mut info = crate::media::ProbeInfo { captured: ph.captured.clone(), ..Default::default() };
        info.meta.focus = Some(fp(0.3));
        let ops = fill_missing_meta(PhotoId(1), &ph, &info);
        let Some(Op::SetMeta { meta, .. }) = ops.first() else { panic!("{ops:?}") };
        assert_eq!(meta.focus, Some(fp(0.3)));
        ph.meta.focus = Some(fp(0.1));
        assert!(fill_missing_meta(PhotoId(1), &ph, &info).is_empty(), "a known focus point is kept");
    }

    fn lens() -> lightcraft_develop::EmbeddedLens {
        lightcraft_develop::EmbeddedLens {
            warp: Some(lightcraft_develop::EmbeddedWarp { planes: [[1.0, -0.1, 0.03, 0.0, 0.0, 0.0]; 3], ..Default::default() }),
            vignette: None,
            stages: Default::default(),
        }
    }
    fn old_rw2() -> lightcraft_catalog::Photo {
        // as an older build catalogued it: no lens data, correction off
        lightcraft_catalog::Photo::new(
            lightcraft_catalog::PhotoId(1),
            Source::File { path: "/x.rw2".into() },
            "x.rw2",
            "RW2",
            4592,
            3448,
            "2026-10-06T00:00:00",
        )
    }

    #[test]
    fn reload_adds_lens_data_and_turns_the_correction_on_for_an_untouched_photo() {
        let ph = old_rw2();
        let info = crate::media::ProbeInfo { embedded_lens: Some(lens()), ..Default::default() };
        let ops = lens_ops(ph.id, &ph, &info);
        assert!(matches!(ops.first(), Some(Op::SetEmbeddedLens { lens: Some(_), .. })), "{ops:?}");
        let Some(Op::SetDevelop { settings, .. }) = ops.get(1) else { panic!("{ops:?}") };
        assert!(settings.optics.lens_profile);
        // once applied, the photo is still "unedited" and has what import would have given it
        let mut c = lightcraft_catalog::Catalog::default();
        c.apply(Op::AddPhoto { photo: Box::new(ph) }).unwrap();
        for op in ops {
            c.apply(op).unwrap();
        }
        let after = c.photo(lightcraft_catalog::PhotoId(1)).unwrap();
        assert_eq!(after.embedded_lens, Some(lens()));
        assert!(!after.is_edited());
        assert_eq!(*after.develop, after.camera_defaults());
        assert!(lens_ops(after.id, after, &info).is_empty(), "nothing more to do the second time");
    }

    #[test]
    fn reload_adds_lens_data_but_keeps_an_edited_photos_settings() {
        let mut ph = old_rw2();
        let mut d = (*ph.develop).clone();
        d.light.exposure = 0.7;
        ph.develop = std::sync::Arc::new(d);
        assert!(ph.is_edited());
        let info = crate::media::ProbeInfo { embedded_lens: Some(lens()), ..Default::default() };
        let ops = lens_ops(ph.id, &ph, &info);
        assert!(matches!(ops.as_slice(), [Op::SetEmbeddedLens { lens: Some(_), .. }]), "{ops:?}");
    }

    #[test]
    fn reload_leaves_a_photo_whose_user_switched_the_correction_off_alone() {
        let mut ph = old_rw2();
        ph.embedded_lens = Some(lens());
        let mut d = ph.camera_defaults();
        d.optics.lens_profile = false;
        ph.develop = std::sync::Arc::new(d);
        let info = crate::media::ProbeInfo { embedded_lens: Some(lens()), ..Default::default() };
        assert!(lens_ops(ph.id, &ph, &info).is_empty());
    }

    /// End to end on a public Panasonic raw (skipped without the corpus): a catalog from before the lens data was read
    /// gets it on Reload, the render changes to the corrected one, and Undo takes it all back.
    #[test]
    fn corpus_reload_corrects_an_rw2_catalogued_without_lens_data() {
        let root = std::env::var_os("LIGHTCRAFT_CORPUS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus"));
        let f = root.join("raw/rw2-panasonic-gx80.rw2");
        if !f.exists() {
            lightcraft_testkit::skip!(Corpus, "{} absent", f.display());
            return;
        }
        let mut s = Session::new().with_fs();
        s.execute("library.import", &json!({"paths": [f.to_string_lossy()]})).unwrap();
        let id = s.active().unwrap();
        let ph = s.catalog.photo(id).unwrap().clone();
        assert!(ph.embedded_lens.is_some() && ph.develop.optics.lens_profile, "import reads it");
        // back to how 0.4.0 catalogued it
        let mut old = (*ph.develop).clone();
        old.optics.lens_profile = false;
        s.catalog.apply(Op::SetEmbeddedLens { id, lens: None }).unwrap();
        s.catalog.apply(Op::SetDevelop { id, settings: std::sync::Arc::new(old), label: "x".into(), edited: None }).unwrap();
        s.media.forget(id);
        let before = s.render_now(id, 160, 120).unwrap().image.data;
        let r = s.execute("photo.reload", &json!({"ids": [id.0]})).unwrap();
        assert_eq!(r["reloaded"], json!([id.0]));
        let now = s.catalog.photo(id).unwrap().clone();
        assert_eq!(now.embedded_lens, ph.embedded_lens);
        assert!(now.develop.optics.lens_profile && !now.is_edited());
        assert_ne!(s.render_now(id, 160, 120).unwrap().image.data, before, "the corrected render differs");
        s.execute("edit.undo", &json!({})).unwrap();
        let undone = s.catalog.photo(id).unwrap();
        assert!(undone.embedded_lens.is_none() && !undone.develop.optics.lens_profile, "one undo step");
    }
}
