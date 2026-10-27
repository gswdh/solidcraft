//! Other CAD formats read through [cadmpeg](https://github.com/cadmpeg/cadmpeg): FreeCAD
//! `.FCStd`, Rhino `.3dm` and Fusion `.f3d` / `.f3z`. cadmpeg decodes the file into its neutral
//! model and writes that as STEP, which the Import feature then reads and keeps like any STEP
//! file. A `.f3z` (several linked `.f3d` documents in one archive) decodes as one merged model.
//!
//! Only those readers are compiled in (the workspace enables cadmpeg-registry's `fcstd`, `rhino`,
//! `f3d` and `step` features alone). cadmpeg's other commercial-format codecs, and its `sat`
//! codec (a ShapeManager stream decoder that reads ACIS as one dialect), are not built.
//!
//! This module is the importer seam: the one place cadmpeg is called, returning STEP text plus
//! warnings. A cadmpeg-based STEP reader could be added here later and chosen by a setting, to
//! compare with our own STEP reader, without changing that reader.

/// File extensions read through cadmpeg (lower case).
pub const NATIVE_CAD_EXTS: [&str; 4] = ["fcstd", "3dm", "f3d", "f3z"];

/// Is this a FreeCAD, Rhino or Fusion file name (any case)?
pub fn is_native_cad_path(path: &str) -> bool {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| NATIVE_CAD_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Restate a FreeCAD, Rhino or Fusion file (`file` is its name) as STEP text, with the losses
/// and warnings cadmpeg reported. Panics inside cadmpeg become errors.
pub fn native_cad_to_step(bytes: &[u8], file: &str) -> Result<(String, Vec<String>), String> {
    let name = std::path::Path::new(file).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| file.to_string());
    solidcraft_kernel::guard("cadmpeg import", || convert::to_step(&name, bytes).map_err(solidcraft_kernel::KernelError::Invalid)).map_err(
        |e| match e {
            solidcraft_kernel::KernelError::Invalid(m) => m,
            e => e.to_string(),
        },
    )
}

mod convert {
    use std::io::Cursor;

    use cadmpeg_core::decode::{DecodeArena, DecodeContext, DecodeDepth, DecodeMode, DecodePolicy, ResourceLimits, View};
    use cadmpeg_ir::codec::write::{EncodeInput, target::TargetRequest};
    use cadmpeg_ir::codec::{DecodeFailure, DecodeOptions, DecodeResult};
    use cadmpeg_ir::validate::validate_neutral_for_decode;
    use cadmpeg_registry::{DETECTION_PREFIX_LEN, Format, InputCatalog, ResolveSourceError, ResolvedSource, build_encoder, forced_input};

    /// The codecs we accept (cadmpeg codec ids), whatever else the registry recognises.
    const ACCEPTED: [&str; 3] = ["fcstd", "rhino", "f3d"];

    /// Caps for hostile input: cadmpeg's tight `service` profile, with our STEP size limit on
    /// the input. The service profile already admits a detailed part's 8 million entities:
    /// cadmpeg 0.6.3 dropped the collection-item budget that refused one at a million (#1783)
    /// and the work-unit budget that went with it.
    pub(super) fn policy() -> DecodePolicy {
        DecodePolicy {
            mode: DecodeMode::Salvage,
            limits: ResourceLimits { max_input_bytes: crate::MAX_STEP_BYTES as u64, ..ResourceLimits::service() },
        }
    }

    /// Decode `bytes` (a file called `name`, extension `ext`) with the codec cadmpeg detects from
    /// the bytes, else the one the extension names, if it is one of `accepted`. Returns the codec id
    /// and the decoded model.
    pub(super) fn decode(name: &str, ext: &str, bytes: &[u8], accepted: &[&str]) -> Result<(String, DecodeResult), String> {
        let policy = policy();
        let catalog = InputCatalog::with_builtins();
        let codec = {
            let arena = DecodeArena::new();
            let (ctx, _) = DecodeContext::from_root_bytes(&[], &arena, &policy).map_err(|e| e.to_string())?;
            let prefix = bytes.get(..DETECTION_PREFIX_LEN).unwrap_or(bytes);
            let resolved = catalog.resolve_seekable_source(&ctx, View::over_retained(prefix), &mut Cursor::new(bytes), None);
            let resolved = match resolved {
                Ok(ResolvedSource::Unrecognized | ResolvedSource::Cadir) | Err(ResolveSourceError::Ambiguous(_)) => {
                    // cadmpeg names the Fusion codec `f3d`; `.f3z` is its multi-document archive.
                    let forced = if ext == "f3z" { "f3d" } else { ext };
                    match forced_input(forced).map_err(|e| e.to_string())? {
                        Some(forced) => catalog.resolve_source(&ctx, View::over_retained(prefix), Some(forced)),
                        None => Ok(ResolvedSource::Unrecognized),
                    }
                }
                other => other,
            };
            ctx.finish_session().map_err(|e| e.to_string())?;
            match resolved.map_err(|e| format!("{name}: {e}"))? {
                ResolvedSource::Native { codec, .. } => codec,
                _ => return Err(format!("{name}: unrecognised file contents")),
            }
        };
        let format = codec.id().as_str().to_string();
        if !accepted.contains(&format.as_str()) {
            return Err(format!("{name}: {format} files are not supported (FreeCAD .FCStd, Rhino .3dm and Fusion .f3d/.f3z only)"));
        }

        // Decode in memory under the caps, as deep as the STEP writer reads: solids and products,
        // not the design history, which the import does not keep.
        let options = DecodeOptions { depth: DecodeDepth::Geometry, policy };
        let decoded = codec.decode(&mut Cursor::new(bytes), &options).map_err(|e| match e {
            DecodeFailure::Codec(e) => format!("{name}: could not read this {format} file: {e}"),
            other => format!("{name}: {other}"),
        })?;
        Ok((format, decoded))
    }

    pub(super) fn to_step(name: &str, bytes: &[u8]) -> Result<(String, Vec<String>), String> {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if !super::NATIVE_CAD_EXTS.contains(&ext.as_str()) {
            return Err(format!("{name}: not supported (FreeCAD .FCStd, Rhino .3dm and Fusion .f3d/.f3z only)"));
        }
        let (format, decoded) = match decode(name, &ext, bytes, &ACCEPTED) {
            Ok(d) => d,
            Err(why) if ext == "f3z" => return f3z_root_only(name, bytes, &why),
            Err(e) => return Err(e),
        };
        let policy = policy();
        let (ir, report, fidelity) = decoded.into_parts();
        if !report.geometry_transferred() {
            return Err(format!("{name}: no geometry could be read from this {format} file"));
        }
        let mut warnings: Vec<String> = report.losses.iter().map(|l| format!("{}: {}", l.code, l.message)).collect();

        // Validate the neutral model, as cadmpeg's own conversion does.
        let arena = DecodeArena::new();
        let ctx = DecodeContext::for_loaded_input(&arena, &policy, bytes.len() as u64).map_err(|e| e.to_string())?;
        let validation = validate_neutral_for_decode(&ctx, &ir, Vec::new()).map_err(|e| e.to_string())?;
        ctx.finish_session().map_err(|e| e.to_string())?;
        warnings.extend(validation.findings.iter().map(|f| format!("{}: {}", f.severity, f.message)));
        if !validation.is_ok() {
            return Err(format!("{name}: the {format} model is invalid ({} errors)", validation.error_count()));
        }

        // Write it as STEP AP214.
        let plan = build_encoder(Format::Step)
            .plan(EncodeInput::new(&ir, Some(&fidelity)), TargetRequest::Explicit("ap214"))
            .map_err(|e| format!("{name}: could not restate as STEP: {e}"))?;
        let losses = &plan.report().losses;
        if losses.iter().any(|l| l.code.to_string() == "step/export.no-exportable-solids") {
            return Err(format!("{name}: this {format} file has no solids that can be imported"));
        }
        warnings.extend(losses.iter().map(|l| format!("{}: {}", l.code, l.message)));
        let mut out = Vec::new();
        plan.write_to(&mut out).map_err(|e| e.to_string())?;
        let step = String::from_utf8(out).map_err(|e| e.to_string())?;
        Ok((super::name_unnamed_products(step, name), warnings))
    }

    /// cadmpeg could not read a `.f3z` as one model (`why`): read its root design alone, as the
    /// `.f3d` it is, and say which linked designs that leaves out. cadmpeg 0.6.6 refuses an
    /// archive whose linked design holds a decal image, which a part exported with its
    /// references easily does.
    fn f3z_root_only(name: &str, bytes: &[u8], why: &str) -> Result<(String, Vec<String>), String> {
        let why = why.strip_prefix(name).and_then(|w| w.strip_prefix(": ")).unwrap_or(why);
        let (member, member_bytes, linked) =
            super::f3z_root_design(bytes).map_err(|e| format!("{name}: could not read this f3z archive ({why}), nor its root design alone ({e})"))?;
        let (step, rest) = to_step(&member, &member_bytes)
            .map_err(|e| format!("{name}: could not read this f3z archive ({why}), nor its root design {member} alone ({e})"))?;
        let left_out = if linked.is_empty() { String::new() } else { format!("; its linked designs were not read: {}", linked.join(", ")) };
        let mut warnings =
            vec![format!("f3z: the archive could not be read as one model ({why}); its root design {member} was read alone{left_out}")];
        warnings.extend(rest);
        Ok((step, warnings))
    }
}

/// The root design of a `.f3z` archive, from its `Manifest.json` and `DesignDescription.json`:
/// the member's display name (as a `.f3d` file name), its bytes, and the display names of the
/// other designs in the archive. A drawing root (`.f2d`) stands for the design it is derived
/// from. Members are read under [`crate::MAX_STEP_BYTES`].
fn f3z_root_design(bytes: &[u8]) -> Result<(String, Vec<u8>, Vec<String>), String> {
    use std::io::Read;
    type Archive<'a> = zip::ZipArchive<std::io::Cursor<&'a [u8]>>;
    fn member(zip: &mut Archive<'_>, entry: &str) -> Result<Vec<u8>, String> {
        let file = zip.by_name(entry).map_err(|e| format!("{entry}: {e}"))?;
        let limit = crate::MAX_STEP_BYTES;
        if file.size() > limit as u64 {
            return Err(format!("{entry}: too large ({} MB, limit {} MB)", file.size() >> 20, limit >> 20));
        }
        let mut out = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut out).map_err(|e| format!("{entry}: {e}"))?;
        if out.len() > limit {
            return Err(format!("{entry}: too large (limit {} MB)", limit >> 20));
        }
        Ok(out)
    }
    fn json(zip: &mut Archive<'_>, entry: &str) -> Result<serde_json::Value, String> {
        serde_json::from_slice(&member(zip, entry)?).map_err(|e| format!("{entry}: {e}"))
    }

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("not a ZIP archive: {e}"))?;
    let manifest = json(&mut zip, "Manifest.json")?;
    let root = manifest["root"].as_str().ok_or("Manifest.json names no root")?.to_string();
    let description = json(&mut zip, "DesignDescription.json")?;
    let objects = description["designDescription"]["designGraphs"][0]["designObjects"].as_array().ok_or("DesignDescription.json lists no designs")?;
    let by_path = |path: &str| objects.iter().find(|o| o["relativePath"].as_str() == Some(path));
    let mut design = by_path(&root);
    if design.is_some_and(|o| o["contentType"].as_str() == Some("f2d")) {
        // The drawing's DERIVED reference is the design it draws.
        let derived = design
            .and_then(|o| o["references"].as_array())
            .into_iter()
            .flatten()
            .find(|r| r["type"].as_str() == Some("DERIVED"))
            .and_then(|r| r["ids"][0].as_i64());
        design = derived.and_then(|id| objects.iter().find(|o| o["id"].as_i64() == Some(id)));
    }
    let design = design.filter(|o| o["contentType"].as_str() == Some("f3d")).ok_or("no root design in DesignDescription.json")?;
    let path = design["relativePath"].as_str().ok_or("the root design has no path")?;
    let stem = path.rsplit_once('.').map(|(s, _)| s).unwrap_or(path);
    let friendly = design["friendlyName"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(stem);
    let linked: Vec<String> = objects
        .iter()
        .filter(|o| o["contentType"].as_str() == Some("f3d") && o["relativePath"].as_str() != Some(path))
        .filter_map(|o| o["friendlyName"].as_str().filter(|n| !n.trim().is_empty()).map(str::to_string))
        .collect();
    let bytes = member(&mut zip, path)?;
    Ok((format!("{friendly}.f3d"), bytes, linked))
}

/// cadmpeg's STEP writer calls a product it has no name for `cadmpeg_model`; name it after the
/// file instead (as plain STEP text: letters, digits, spaces, `-`, `_` and `.` only).
fn name_unnamed_products(step: String, file: &str) -> String {
    const PLACEHOLDER: &str = "'cadmpeg_model'";
    let stem: String = std::path::Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.')).collect())
        .unwrap_or_default();
    if stem.trim().is_empty() || !step.contains(PLACEHOLDER) { step } else { step.replace(PLACEHOLDER, &format!("'{stem}'")) }
}

#[cfg(test)]
mod tests;
