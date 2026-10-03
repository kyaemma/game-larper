//! Reads Discord's detectable-games JSON straight into [`GameDefinition`]s.
//!
//! The catalog is a ~13 MB array. Walking a `serde_json::Value` first would keep the whole
//! document tree alive next to the finished records, so entries are built while the array is
//! visited and nothing but the kept fields is allocated.
//!
//! Leniency matches what the old tree walk accepted, because Discord's data is not ours:
//! unknown fields are ignored, a field of the wrong type counts as absent, a repeated key keeps
//! its last value, and an entry that is not a valid game is skipped without failing the file.
//! Only invalid JSON syntax is an error.

use std::fmt;

use serde::Deserialize;
use serde::de::{Deserializer, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::catalog::{ExecutableChoice, GameDefinition};

/// What a catalog document turned out to be.
pub(crate) enum Catalog {
    Games(Vec<GameDefinition>),
    NotAnArray,
}

impl<'de> Deserialize<'de> for Catalog {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CatalogVisitor)
    }
}

/// Visitor methods for values a type does not want. Each skips the value and yields `$none`.
macro_rules! skip_scalars {
    ($none:expr) => {
        fn visit_bool<E: Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($none)
        }
        fn visit_i64<E: Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($none)
        }
        fn visit_u64<E: Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($none)
        }
        fn visit_f64<E: Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($none)
        }
        fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
            Ok($none)
        }
    };
}

/// Consume a sequence or map the visitor does not want, so the parser stays in step.
fn drain_seq<'de, A: SeqAccess<'de>>(seq: &mut A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A: MapAccess<'de>>(map: &mut A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

struct CatalogVisitor;

impl<'de> Visitor<'de> for CatalogVisitor {
    type Value = Catalog;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    skip_scalars!(Catalog::NotAnArray);

    fn visit_str<E: Error>(self, _: &str) -> Result<Catalog, E> {
        Ok(Catalog::NotAnArray)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Catalog, A::Error> {
        drain_map(&mut map)?;
        Ok(Catalog::NotAnArray)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Catalog, A::Error> {
        let mut games = Vec::new();
        while let Some(Entry(entry)) = seq.next_element()? {
            games.extend(entry);
        }
        Ok(Catalog::Games(games))
    }
}

/// One array element: a game, or nothing when the element is not a valid one.
struct Entry(Option<GameDefinition>);

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EntryVisitor)
    }
}

struct EntryVisitor;

impl<'de> Visitor<'de> for EntryVisitor {
    type Value = Entry;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a catalog entry")
    }

    skip_scalars!(Entry(None));

    fn visit_str<E: Error>(self, _: &str) -> Result<Entry, E> {
        Ok(Entry(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Entry, A::Error> {
        drain_seq(&mut seq)?;
        Ok(Entry(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entry, A::Error> {
        let mut id = None;
        let mut name = None;
        let mut aliases = Vec::new();
        let mut executable = ExecutableChoice::default();
        let mut steam_app_id = None;
        let mut icon_hash = None;
        let mut icon = None;
        while let Some(field) = map.next_key()? {
            match field {
                Field::Id => id = map.next_value::<Text>()?.0,
                Field::Name => name = map.next_value::<Text>()?.0,
                Field::Aliases => aliases = map.next_value::<Aliases>()?.0,
                Field::Executables => executable = map.next_value::<Executables>()?.0,
                Field::ThirdPartySkus => steam_app_id = map.next_value::<SteamSku>()?.0,
                Field::IconHash => icon_hash = map.next_value::<Text>()?.0,
                Field::Icon => icon = map.next_value::<Text>()?.0,
                Field::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Entry(build_game(
            id,
            name,
            aliases,
            executable,
            steam_app_id,
            icon_hash.or(icon),
        )))
    }
}

fn build_game(
    id: Option<String>,
    name: Option<String>,
    aliases: Vec<String>,
    executable: ExecutableChoice,
    steam_app_id: Option<String>,
    icon_hash: Option<String>,
) -> Option<GameDefinition> {
    let id = id?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let name = name?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(GameDefinition {
        id,
        name,
        aliases,
        steam_app_id,
        icon_hash,
        executable,
    })
}

enum Field {
    Id,
    Name,
    Aliases,
    Executables,
    ThirdPartySkus,
    IconHash,
    Icon,
    Other,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = Field;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a field name")
            }

            fn visit_str<E: Error>(self, key: &str) -> Result<Field, E> {
                Ok(match key {
                    "id" => Field::Id,
                    "name" => Field::Name,
                    "aliases" => Field::Aliases,
                    "executables" => Field::Executables,
                    "third_party_skus" => Field::ThirdPartySkus,
                    "icon_hash" => Field::IconHash,
                    "icon" => Field::Icon,
                    _ => Field::Other,
                })
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// A string value, or `None` for any other type.
struct Text(Option<String>);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TextVisitor)
    }
}

struct TextVisitor;

impl<'de> Visitor<'de> for TextVisitor {
    type Value = Text;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a string")
    }

    skip_scalars!(Text(None));

    fn visit_str<E: Error>(self, value: &str) -> Result<Text, E> {
        Ok(Text(Some(value.to_string())))
    }

    fn visit_string<E: Error>(self, value: String) -> Result<Text, E> {
        Ok(Text(Some(value)))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Text, A::Error> {
        drain_seq(&mut seq)?;
        Ok(Text(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Text, A::Error> {
        drain_map(&mut map)?;
        Ok(Text(None))
    }
}

/// A boolean value, or `None` for any other type.
struct Flag(Option<bool>);

impl<'de> Deserialize<'de> for Flag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FlagVisitor)
    }
}

struct FlagVisitor;

impl<'de> Visitor<'de> for FlagVisitor {
    type Value = Flag;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a boolean")
    }

    fn visit_bool<E: Error>(self, value: bool) -> Result<Flag, E> {
        Ok(Flag(Some(value)))
    }

    fn visit_i64<E: Error>(self, _: i64) -> Result<Flag, E> {
        Ok(Flag(None))
    }

    fn visit_u64<E: Error>(self, _: u64) -> Result<Flag, E> {
        Ok(Flag(None))
    }

    fn visit_f64<E: Error>(self, _: f64) -> Result<Flag, E> {
        Ok(Flag(None))
    }

    fn visit_unit<E: Error>(self) -> Result<Flag, E> {
        Ok(Flag(None))
    }

    fn visit_str<E: Error>(self, _: &str) -> Result<Flag, E> {
        Ok(Flag(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Flag, A::Error> {
        drain_seq(&mut seq)?;
        Ok(Flag(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Flag, A::Error> {
        drain_map(&mut map)?;
        Ok(Flag(None))
    }
}

/// The non-blank string elements of an array; anything else yields nothing.
struct Aliases(Vec<String>);

impl<'de> Deserialize<'de> for Aliases {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AliasesVisitor)
    }
}

struct AliasesVisitor;

impl<'de> Visitor<'de> for AliasesVisitor {
    type Value = Aliases;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array of aliases")
    }

    skip_scalars!(Aliases(Vec::new()));

    fn visit_str<E: Error>(self, _: &str) -> Result<Aliases, E> {
        Ok(Aliases(Vec::new()))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Aliases, A::Error> {
        drain_map(&mut map)?;
        Ok(Aliases(Vec::new()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Aliases, A::Error> {
        let mut aliases = Vec::new();
        while let Some(Text(alias)) = seq.next_element()? {
            aliases.extend(alias.filter(|alias| !alias.trim().is_empty()));
        }
        Ok(Aliases(aliases))
    }
}

/// The best Windows executable of an `executables` array, and how many rules it listed.
struct Executables(ExecutableChoice);

impl<'de> Deserialize<'de> for Executables {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ExecutablesVisitor)
    }
}

struct ExecutablesVisitor;

impl<'de> Visitor<'de> for ExecutablesVisitor {
    type Value = Executables;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array of executables")
    }

    skip_scalars!(Executables(ExecutableChoice::default()));

    fn visit_str<E: Error>(self, _: &str) -> Result<Executables, E> {
        Ok(Executables(ExecutableChoice::default()))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Executables, A::Error> {
        drain_map(&mut map)?;
        Ok(Executables(ExecutableChoice::default()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Executables, A::Error> {
        let mut choice = ExecutableChoice::default();
        while let Some(Executable(rule)) = seq.next_element()? {
            if let Some((name, is_launcher)) = rule {
                choice.offer(&name, is_launcher);
            }
        }
        Ok(Executables(choice))
    }
}

/// One `executables` element as (name, is_launcher), only when it is a named Windows rule.
struct Executable(Option<(String, bool)>);

impl<'de> Deserialize<'de> for Executable {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ExecutableVisitor)
    }
}

struct ExecutableVisitor;

impl<'de> Visitor<'de> for ExecutableVisitor {
    type Value = Executable;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an executable rule")
    }

    skip_scalars!(Executable(None));

    fn visit_str<E: Error>(self, _: &str) -> Result<Executable, E> {
        Ok(Executable(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Executable, A::Error> {
        drain_seq(&mut seq)?;
        Ok(Executable(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Executable, A::Error> {
        let (mut os, mut name, mut is_launcher) = (None, None, None);
        while let Some(key) = map.next_key::<ExecutableField>()? {
            match key {
                ExecutableField::Os => os = map.next_value::<Text>()?.0,
                ExecutableField::Name => name = map.next_value::<Text>()?.0,
                ExecutableField::IsLauncher => is_launcher = map.next_value::<Flag>()?.0,
                ExecutableField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let windows = os.is_some_and(|os| os.eq_ignore_ascii_case("win32"));
        Ok(Executable(match name {
            Some(name) if windows => Some((name, is_launcher == Some(true))),
            _ => None,
        }))
    }
}

enum ExecutableField {
    Os,
    Name,
    IsLauncher,
    Other,
}

impl<'de> Deserialize<'de> for ExecutableField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = ExecutableField;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a field name")
            }

            fn visit_str<E: Error>(self, key: &str) -> Result<ExecutableField, E> {
                Ok(match key {
                    "os" => ExecutableField::Os,
                    "name" => ExecutableField::Name,
                    "is_launcher" => ExecutableField::IsLauncher,
                    _ => ExecutableField::Other,
                })
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// The first Steam SKU with a short numeric id, from a `third_party_skus` array.
struct SteamSku(Option<String>);

impl<'de> Deserialize<'de> for SteamSku {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SteamSkuVisitor)
    }
}

struct SteamSkuVisitor;

impl<'de> Visitor<'de> for SteamSkuVisitor {
    type Value = SteamSku;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array of SKUs")
    }

    skip_scalars!(SteamSku(None));

    fn visit_str<E: Error>(self, _: &str) -> Result<SteamSku, E> {
        Ok(SteamSku(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SteamSku, A::Error> {
        drain_map(&mut map)?;
        Ok(SteamSku(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<SteamSku, A::Error> {
        let mut found = None;
        // Later SKUs are still consumed; only the first valid Steam id counts.
        while let Some(Sku(id)) = seq.next_element()? {
            if found.is_none() {
                found = id;
            }
        }
        Ok(SteamSku(found))
    }
}

/// One SKU element: its id when it is a Steam SKU with a short numeric id.
struct Sku(Option<String>);

impl<'de> Deserialize<'de> for Sku {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SkuVisitor)
    }
}

struct SkuVisitor;

impl<'de> Visitor<'de> for SkuVisitor {
    type Value = Sku;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a SKU")
    }

    skip_scalars!(Sku(None));

    fn visit_str<E: Error>(self, _: &str) -> Result<Sku, E> {
        Ok(Sku(None))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Sku, A::Error> {
        drain_seq(&mut seq)?;
        Ok(Sku(None))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Sku, A::Error> {
        let (mut distributor, mut id) = (None, None);
        while let Some(key) = map.next_key::<SkuField>()? {
            match key {
                SkuField::Distributor => distributor = map.next_value::<Text>()?.0,
                SkuField::Id => id = map.next_value::<Text>()?.0,
                SkuField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let steam = distributor.is_some_and(|name| name.eq_ignore_ascii_case("steam"));
        Ok(Sku(id.filter(|id| {
            steam
                && !id.is_empty()
                && id.len() <= 12
                && id.bytes().all(|byte| byte.is_ascii_digit())
        })))
    }
}

enum SkuField {
    Distributor,
    Id,
    Other,
}

impl<'de> Deserialize<'de> for SkuField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = SkuField;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a field name")
            }

            fn visit_str<E: Error>(self, key: &str) -> Result<SkuField, E> {
                Ok(match key {
                    "distributor" => SkuField::Distributor,
                    "id" => SkuField::Id,
                    _ => SkuField::Other,
                })
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}
