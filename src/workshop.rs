use std::{
    borrow::Cow,
    fmt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{self, bail, ContextCompat};
use fs_err::PathExt;
use itertools::Itertools;
use relative_path::PathExt as RelPathExt;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tracing::{debug, info, warn};

use crate::{
    config::{Config, WorkshopItemConfig},
    defines::WORKSHOP_METADATA_FILENAME,
    ext::{SteamworksClient, SteamworksSingleClient, UGCBlockingExt},
};

#[serde_as]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppId(#[serde_as(as = "DisplayFromStr")] pub u32);
impl From<u32> for AppId {
    fn from(id: u32) -> Self {
        AppId(id)
    }
}

impl Into<steamworks::AppId> for AppId {
    fn into(self) -> steamworks::AppId {
        self.0.into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tag(Cow<'static, str>);

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Tag {
    pub fn new(s: impl Into<Cow<'static, str>>) -> eyre::Result<Self> {
        let s = s.into();
        Self::is_valid_tag(&s)?;

        Ok(Self(s))
    }
    /// https://partner.steamgames.com/doc/api/ISteamUGC#SetItemTags
    fn is_valid_tag(s: impl AsRef<str>) -> eyre::Result<()> {
        if s.as_ref().is_empty() {
            bail!("Empty tags are not allowed")
        }

        if !s.as_ref().len() < 256 {
            bail!("Tag can only have a max length of 255 characters")
        }

        if s.as_ref()
            .chars()
            .any(|c| !(c != ',' && (c.is_ascii_graphic() || c.is_ascii_whitespace())))
        {
            bail!("Tag contains invalid characters")
        };

        Ok(())
    }
    pub fn is_in_predefined_tags(&self, tags: &[Tag]) -> bool {
        tags.iter().any(|it| it.0 == self.0)
    }
}

pub fn check_tags_are_predefined(tags: &[Tag], predefined: &[Tag]) -> eyre::Result<()> {
    tags.iter()
        .map(|it| {
            it.is_in_predefined_tags(predefined)
                .then_some(())
                .with_context(|| {
                    eyre::eyre!(
                        "`{}` is not a predefined tag. Available tags are: {}",
                        it,
                        predefined.iter().join(", ")
                    )
                })
        })
        .find(|it| it.is_err())
        .unwrap_or(Ok(()))
}

impl AsRef<str> for Tag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

pub fn is_valid_preview_type(path: impl AsRef<Path>) -> eyre::Result<()> {
    match infer::get_from_path(path)?
        .context("Unknown file type")?
        .mime_type()
    {
        "image/jpeg" | "image/gif" | "image/png" => Ok(()),
        mime_type => {
            bail!(
                "Invalid preview filetype `{}`: Only png, jpeg, and gif are allowed",
                mime_type
            );
        }
    }
}

pub fn steamworks_client_init(
    app_id: impl Into<steamworks::AppId>,
) -> eyre::Result<(SteamworksClient, SteamworksSingleClient)> {
    Ok(steamworks::Client::init_app(app_id).map_err(|err| {
        eyre::eyre!(
            "{}",
            match err {
                // Display for this variant gives "Some Other Error" which is not helpful. Have to get the inner string like this
                steamworks::SteamAPIInitError::FailedGeneric(err) => err,
                err => format!("{err}"),
            }
        )
    })?)
}

/// Both `from` and `to` are paths to directory.
/// Make a copy of data in `from` in `to` while ignoring files matched in the glob.
pub fn copy_filtered_content<I, O>(
    from: I,
    to: O,
    globs: Option<&[impl AsRef<str>]>,
    ignore_files: Option<&[impl AsRef<Path>]>,
) -> eyre::Result<()>
where
    I: AsRef<Path>,
    O: AsRef<Path>,
{
    let mut overrides = ignore::overrides::OverrideBuilder::new(from.as_ref());
    overrides.add(&format!("!{}", WORKSHOP_METADATA_FILENAME))?;

    if let Some(globs) = globs {
        for glob in globs {
            overrides.add(glob.as_ref())?;
        }
    }

    let mut walk_builder = ignore::WalkBuilder::new(from.as_ref());
    walk_builder.overrides(overrides.build()?);

    if let Some(ignore_files) = ignore_files {
        for ignore_file in ignore_files {
            walk_builder.add_ignore(ignore_file.as_ref());
        }
    }

    for entry in walk_builder
        .build()
        .inspect(|it| {
            _ = it.as_ref().inspect_err(|err| warn!("{err}"));
        })
        .filter_map(|it| it.ok())
        .filter(|it| it.depth() != 0)
    {
        if let Some(file_type) = entry.file_type() {
            let relative_entry_path = entry.path().relative_to(&from.as_ref())?;
            let proxy_path = relative_entry_path.to_path(&to.as_ref());

            if file_type.is_dir() {
                fs_err::create_dir_all(proxy_path)?;
            } else if file_type.is_file() {
                debug!(file = %relative_entry_path, "Adding to item content");
                fs_err::copy(entry.path().fs_err_canonicalize()?, &proxy_path)?;
            }
        }
    }

    Ok(())
}

pub fn create_item_with_metadata_file(
    client: &SteamworksClient,
    single: &SteamworksSingleClient,
    app_id: impl Into<steamworks::AppId>,
    content_path: impl AsRef<Path>,
    tags: &[Tag],
) -> eyre::Result<(steamworks::PublishedFileId, bool)> {
    let app_id = app_id.into();
    let (file_id, agreement) =
        client
            .ugc()
            .create_item_blocking(single, app_id, steamworks::FileType::Community)?;

    info!(item_id = file_id.0, "Workshop item created");

    _ = WorkshopItemConfig {
        app_id: app_id.0,
        item_id: file_id.0,
        tags: tags.to_owned(),
    }
    .store_path(content_path.as_ref().join(WORKSHOP_METADATA_FILENAME))?;

    Ok((file_id, agreement))
}

/// Steam AppId of DayZ
pub const DAYZ_APP_ID: u32 = 221100;
const META_CPP_FILENAME: &str = "meta.cpp";

/// DayZ mods carry a `meta.cpp` in their root; the game uses `publishedid` in it to
/// identify the workshop item a mod came from, so it must match the actual item id.
///
/// Creates the file in `content_path` if missing, otherwise only replaces the
/// `publishedid` and `timestamp` values in-place, leaving other fields untouched.
///
/// `timestamp` is in .NET `DateTime.ToBinary()` (Utc) format — `0x4000000000000000 | ticks`
/// where ticks are 100ns intervals since 0001-01-01 — matching what DayZ's own
/// publishing tool writes.
pub fn write_dayz_meta_cpp(content_path: impl AsRef<Path>, item_id: u64) -> eyre::Result<()> {
    let content_path = content_path.as_ref();
    let ticks =
        (SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 62_135_596_800) * 10_000_000;
    let timestamp = ticks | (1 << 62);

    let meta_cpp_path = content_path.join(META_CPP_FILENAME);
    let meta_cpp = if meta_cpp_path.exists() {
        let source = fs_err::read_to_string(&meta_cpp_path)?;
        let source = upsert_meta_cpp_field(&source, "publishedid", &item_id.to_string());
        upsert_meta_cpp_field(&source, "timestamp", &timestamp.to_string())
    } else {
        // Fall back to the content folder's name (minus the `@` prefix)
        let name = content_path
            .file_name()
            .and_then(|it| it.to_str())
            .map(|it| it.trim_start_matches('@'))
            .unwrap_or_default();
        format!("protocol = 1;\npublishedid = {item_id};\nname = \"{name}\";\ntimestamp = {timestamp};\n")
    };
    fs_err::write(&meta_cpp_path, meta_cpp)?;

    Ok(())
}

/// Replaces the value of a `key = value;` line in-place, appending it if not present.
fn upsert_meta_cpp_field(source: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let mut lines: Vec<_> = source
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            match trimmed.strip_prefix(key) {
                Some(rest) if rest.trim_start().starts_with('=') => {
                    found = true;
                    format!("{}{key} = {value};", &line[..line.len() - trimmed.len()])
                }
                _ => line.to_owned(),
            }
        })
        .collect();
    if !found {
        lines.push(format!("{key} = {value};"));
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

pub fn open_workshop_page(item_id: u64) -> eyre::Result<()> {
    open::that(format!("steam://url/CommunityFilePage/{}", item_id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_value_in_place() {
        let source = "protocol = 1;\npublishedid = 0;\nname = \"mod\";\ntimestamp = 123;\n";
        let out = upsert_meta_cpp_field(source, "publishedid", "42");
        assert_eq!(
            out,
            "protocol = 1;\npublishedid = 42;\nname = \"mod\";\ntimestamp = 123;\n"
        );
        let out = upsert_meta_cpp_field(&out, "timestamp", "5250952534947387904");
        assert_eq!(
            out,
            "protocol = 1;\npublishedid = 42;\nname = \"mod\";\ntimestamp = 5250952534947387904;\n"
        );
    }

    #[test]
    fn upsert_appends_missing_field() {
        let out = upsert_meta_cpp_field("protocol = 1;\n", "publishedid", "0");
        assert_eq!(out, "protocol = 1;\npublishedid = 0;\n");
    }

    #[test]
    fn upsert_ignores_other_keys_and_comments() {
        let source = "/// comment\nname = \"publishedid fake\";\n";
        let out = upsert_meta_cpp_field(source, "publishedid", "1");
        assert_eq!(
            out,
            "/// comment\nname = \"publishedid fake\";\npublishedid = 1;\n"
        );
    }

    #[test]
    fn timestamp_is_dotnet_to_binary_utc() {
        // DateTime.ToBinary() of 2026-10-03 19:14:12 UTC
        let ticks = (1_791_054_852u64 + 62_135_596_800) * 10_000_000;
        assert_eq!(ticks | (1 << 62), 5_250_952_534_947_387_904);
    }

    #[test]
    fn writes_and_updates_meta_cpp() {
        let dir = tempfile::tempdir().unwrap();
        write_dayz_meta_cpp(dir.path(), 3812840035).unwrap();
        let meta_cpp_path = dir.path().join(META_CPP_FILENAME);
        let meta_cpp = fs_err::read_to_string(&meta_cpp_path).unwrap();
        assert!(meta_cpp.starts_with("protocol = 1;\npublishedid = 3812840035;\nname = \""));
        assert!(meta_cpp.ends_with(";\n"));
        let timestamp: u64 = meta_cpp
            .lines()
            .find_map(|it| it.trim().strip_prefix("timestamp = "))
            .and_then(|it| it.trim_end_matches(';').parse().ok())
            .unwrap();
        assert_eq!(timestamp >> 62, 1);

        // A pre-existing file is only missing-updated, other fields preserved
        write_dayz_meta_cpp(dir.path(), 42).unwrap();
        let meta_cpp = fs_err::read_to_string(&meta_cpp_path).unwrap();
        let get = |key: &str| {
            meta_cpp
                .lines()
                .find(|it| {
                    it.trim_start()
                        .strip_prefix(key)
                        .map_or(false, |rest| rest.trim_start().starts_with('='))
                })
                .unwrap()
                .to_owned()
        };
        assert!(get("publishedid")
            .trim_start_matches("publishedid = ")
            .ends_with("42;"));
        assert!(get("name").contains("\"")); // untouched
        assert!(get("protocol").contains("1")); // untouched
    }
}
