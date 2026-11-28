//! Optional external shader-profile configuration loaded from
//! `VICTORY_SHADER_PROFILE` (or `[paths].shader_profile` in `victory.toml`).
//!
//! When unset or absent, every field defaults to empty/`None`, keeping
//! `victory-gpu` free of title-specific shader hashes, shader symbol substrings,
//! or fixed root-table offsets while still supporting structural heuristics
//! (such as `VICTORY_BAKE_DETECT=shape`).

use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

const PROFILE_ENV: &str = "VICTORY_SHADER_PROFILE";

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShaderProfile {
    #[serde(default)]
    pub(crate) ambient: AmbientProfile,
    #[serde(default)]
    pub(crate) bake: BakeProfile,
    #[serde(default)]
    pub(crate) cull_shaders: Vec<CullShader>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AmbientProfile {
    #[serde(default)]
    pub(crate) resolve_substring: String,
    #[serde(default)]
    pub(crate) radiance_substring: String,
    #[serde(default)]
    pub(crate) air_substring: String,
    #[serde(default)]
    pub(crate) light_binding: Option<(u8, u32)>,
    #[serde(default)]
    pub(crate) depth_binding: Option<(u8, u32)>,
    #[serde(default)]
    pub(crate) radiance_binding: Option<(u8, u32)>,
    #[serde(default)]
    pub(crate) air_binding: Option<(u8, u32)>,
    /// The program that gives reflection rays that found nothing their light
    /// (`crate::ambient::Role::Miss`, for `VICTORY_AMBIENT_SPECULAR`): a
    /// substring of its container, and substrings that, also there, make it
    /// another program (variants of it that are not for reflections).
    #[serde(default)]
    pub(crate) miss_substring: String,
    #[serde(default)]
    pub(crate) miss_not_substrings: Vec<String>,
    /// Where it writes the reflections' image and reads the depth (found by
    /// kind in its tables where these are unset or hold nothing).
    #[serde(default)]
    pub(crate) miss_light_binding: Option<(u8, u32)>,
    #[serde(default)]
    pub(crate) miss_depth_binding: Option<(u8, u32)>,
    /// The values of its first root constant whose dispatches are followed
    /// (the kind of surface its rays are of); those of `miss_alpha_layers`
    /// only with `VICTORY_AMBIENT_SPECULAR_ALPHA=1`. Both empty: every
    /// dispatch.
    #[serde(default)]
    pub(crate) miss_layers: Vec<u32>,
    #[serde(default)]
    pub(crate) miss_alpha_layers: Vec<u32>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BakeProfile {
    #[serde(default)]
    pub(crate) old_pixel_shaders: Vec<String>,
    #[serde(default)]
    pub(crate) pixel_shaders: Vec<String>,
    #[serde(default)]
    pub(crate) geometry_shaders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CullShader {
    pub(crate) hash: String,
    pub(crate) label: String,
}

/// A shader hash as the profile's users compare it: 16 hex digits, lower case. None for
/// anything else.
fn hash(text: &str) -> Option<String> {
    let text = text.trim();
    (text.len() == 16 && text.bytes().all(|b| b.is_ascii_hexdigit())).then(|| text.to_ascii_lowercase())
}

impl ShaderProfile {
    /// The profile `text` describes, its hashes in lower case; the ones that are not
    /// hashes are left out and told.
    pub(crate) fn from_toml_str(text: &str) -> Result<Self, String> {
        let mut profile: Self = toml::from_str(text).map_err(|e| format!("parsing shader profile TOML: {e}"))?;
        for (list, bad) in profile.normalise() {
            tracing::warn!(list, hash = %bad, "shader profile: not a shader hash (16 hex digits); left out");
        }
        Ok(profile)
    }

    /// Puts every hash in lower case and takes out the ones that are not hashes: those,
    /// each with the list it was in.
    fn normalise(&mut self) -> Vec<(&'static str, String)> {
        let mut bad = Vec::new();
        let bake = &mut self.bake;
        for (name, list) in [
            ("bake.old_pixel_shaders", &mut bake.old_pixel_shaders),
            ("bake.pixel_shaders", &mut bake.pixel_shaders),
            ("bake.geometry_shaders", &mut bake.geometry_shaders),
        ] {
            list.retain_mut(|h| match hash(h) {
                Some(good) => {
                    *h = good;
                    true
                }
                None => {
                    bad.push((name, std::mem::take(h)));
                    false
                }
            });
        }
        self.cull_shaders.retain_mut(|c| match hash(&c.hash) {
            Some(good) => {
                c.hash = good;
                true
            }
            None => {
                bad.push(("cull_shaders", std::mem::take(&mut c.hash)));
                false
            }
        });
        bad
    }

    /// What a loaded profile holds: its bake hashes, its cull shaders, and whether it names
    /// any ambient role.
    fn counts(&self) -> (usize, usize, bool) {
        let (bake, ambient) = (&self.bake, &self.ambient);
        let hashes = bake.old_pixel_shaders.len() + bake.pixel_shaders.len() + bake.geometry_shaders.len();
        let roles =
            [&ambient.resolve_substring, &ambient.radiance_substring, &ambient.air_substring, &ambient.miss_substring];
        (hashes, self.cull_shaders.len(), roles.iter().any(|s| !s.is_empty()))
    }

    pub(crate) fn from_file(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read external shader profile; using empty profile");
                return Self::default();
            }
        };
        match Self::from_toml_str(&text) {
            Ok(p) => {
                let (bake_hashes, cull_shaders, ambient_roles) = p.counts();
                tracing::info!(path = %path.display(), bake_hashes, cull_shaders, ambient_roles, "shader profile loaded");
                p
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "invalid external shader profile; using empty profile");
                Self::default()
            }
        }
    }
}

pub(crate) fn current() -> &'static ShaderProfile {
    static PROFILE: OnceLock<ShaderProfile> = OnceLock::new();
    PROFILE.get_or_init(|| match std::env::var_os(PROFILE_ENV).filter(|v| !v.is_empty()) {
        Some(p) => ShaderProfile::from_file(Path::new(&p)),
        None => {
            tracing::info!("no shader profile configured ({PROFILE_ENV} is unset)");
            ShaderProfile::default()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_parses_as_default_profile() {
        let p = ShaderProfile::from_toml_str("").unwrap();
        assert_eq!(p, ShaderProfile::default());
        assert!(p.bake.pixel_shaders.is_empty());
        assert!(p.cull_shaders.is_empty());
        assert!(p.ambient.light_binding.is_none());
    }

    #[test]
    fn custom_profile_parses_all_sections() {
        let text = r#"
[ambient]
resolve_substring = "CS_Test_Resolve"
radiance_substring = "CS_Test_Radiance"
air_substring = "CS_Test_Air"
light_binding = [1, 10]
depth_binding = [0, 20]
radiance_binding = [1, 4]
air_binding = [1, 8]
miss_substring = "CS_Test_Miss_"
miss_not_substrings = ["CS_Test_Miss_Other"]
miss_light_binding = [1, 26]
miss_depth_binding = [0, 250]
miss_layers = [0]
miss_alpha_layers = [1]

[bake]
old_pixel_shaders = ["aaaa000011112222"]
pixel_shaders = ["bbbb000011112222"]
geometry_shaders = ["cccc000011112222"]

[[cull_shaders]]
hash = "dddd000011112222"
label = "test_cull"
"#;
        let p = ShaderProfile::from_toml_str(text).unwrap();
        assert_eq!(p.ambient.resolve_substring, "CS_Test_Resolve");
        assert_eq!(p.ambient.light_binding, Some((1, 10)));
        assert_eq!(p.ambient.depth_binding, Some((0, 20)));
        assert_eq!(p.ambient.miss_substring, "CS_Test_Miss_");
        assert_eq!(p.ambient.miss_not_substrings, ["CS_Test_Miss_Other"]);
        assert_eq!(p.ambient.miss_light_binding, Some((1, 26)));
        assert_eq!(p.ambient.miss_depth_binding, Some((0, 250)));
        assert_eq!((&p.ambient.miss_layers[..], &p.ambient.miss_alpha_layers[..]), (&[0][..], &[1][..]));
        assert_eq!(p.bake.old_pixel_shaders, ["aaaa000011112222"]);
        assert_eq!(p.bake.pixel_shaders, ["bbbb000011112222"]);
        assert_eq!(p.bake.geometry_shaders, ["cccc000011112222"]);
        assert_eq!(p.cull_shaders.len(), 1);
        assert_eq!(p.cull_shaders[0].label, "test_cull");
        assert_eq!(p.counts(), (3, 1, true));
        assert_eq!(ShaderProfile::default().counts(), (0, 0, false));
    }

    #[test]
    fn hashes_are_lower_case_and_what_is_not_a_hash_is_left_out() {
        let text = r#"
[bake]
old_pixel_shaders = ["AAAA000011112222", " 00ff00ff00FF00ff "]
pixel_shaders = ["bbbb00001111222", "bbbb0000111122223", "0xbbbb0000111122", "BBBB00001111222G", ""]
geometry_shaders = ["CcCc000011112222"]

[[cull_shaders]]
hash = "DDDD000011112222"
label = "kept"

[[cull_shaders]]
hash = "not a hash"
label = "left out"
"#;
        let p = ShaderProfile::from_toml_str(text).unwrap();
        assert_eq!(p.bake.old_pixel_shaders, ["aaaa000011112222", "00ff00ff00ff00ff"]);
        assert!(p.bake.pixel_shaders.is_empty());
        assert_eq!(p.bake.geometry_shaders, ["cccc000011112222"]);
        assert_eq!(p.cull_shaders, [CullShader { hash: "dddd000011112222".into(), label: "kept".into() }]);
        assert_eq!(p.counts(), (3, 1, false));
        // what was left out, and from where
        let mut raw: ShaderProfile = toml::from_str(text).unwrap();
        let bad = raw.normalise();
        assert_eq!(bad.len(), 6);
        assert!(bad[..5].iter().all(|b| b.0 == "bake.pixel_shaders"));
        assert_eq!(bad[5], ("cull_shaders", "not a hash".to_owned()));
        assert_eq!(raw, p);
    }

    #[test]
    fn a_malformed_or_missing_profile_is_the_empty_one() {
        // (cut short, a wrong type, an unknown key, a missing one)
        let cases =
            ["[bake", "[bake]\npixel_shaders = 3", "[unknown]\nkey = 1", "[[cull_shaders]]\nhash = \"aaaa000011112222\""];
        for text in cases {
            assert!(ShaderProfile::from_toml_str(text).is_err(), "{text}");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.toml");
        assert_eq!(ShaderProfile::from_file(&path), ShaderProfile::default());
        std::fs::write(&path, "[bake").unwrap();
        assert_eq!(ShaderProfile::from_file(&path), ShaderProfile::default());
        std::fs::write(&path, "[bake]\ngeometry_shaders = [\"CCCC000011112222\"]\n").unwrap();
        assert_eq!(ShaderProfile::from_file(&path).bake.geometry_shaders, ["cccc000011112222"]);
    }
}
