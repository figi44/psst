use std::collections::{BTreeMap, BTreeSet};

use psst_config::{ConfigFile, ProfileSpec, ProfilesFile};
use psst_proto::api::{ApiErrorCode, Profile as ApiProfile, ProfileChange, ProfileEntry, ProfileLayer, ProfilesView};
use serde::Serialize;

use crate::{api::ApiErr, db::Db, state::LiveConfig};

#[derive(Debug)]
pub struct ProfileLayers {
    file: ProfilesFile,
    stored: BTreeMap<String, ProfileSpec>,
}

impl ProfileLayers {
    pub fn load(file: ProfilesFile, db: &Db) -> anyhow::Result<(ProfileLayers, ProfilesFile)> {
        let mut layers = ProfileLayers { file, stored: BTreeMap::new() };
        for (name, stored) in db.profiles()? {
            let checked = stored.and_then(ProfileSpec::try_from).and_then(|spec| {
                let mut proposed = layers.effective();
                proposed.profiles.insert(name.clone(), spec.clone());
                proposed.check().map(|()| spec)
            });
            match checked {
                Ok(spec) => {
                    layers.stored.insert(name, spec);
                }
                Err(err) => tracing::error!("profiles: leaving out the database's profile `{name}`: {err}"),
            }
        }
        let effective = layers.effective();
        Ok((layers, effective))
    }

    pub fn effective(&self) -> ProfilesFile {
        let mut profiles = self.file.clone();
        profiles.profiles.extend(self.stored.iter().map(|(name, spec)| (name.clone(), spec.clone())));
        profiles
    }

    pub fn reload(&mut self, file: ProfilesFile) -> ProfilesFile {
        self.file = file;
        self.effective()
    }

    fn entry(&self, name: &str) -> Option<ProfileEntry> {
        let (layer, profile, file) = match (self.file.profiles.get(name), self.stored.get(name)) {
            (None, None) => return None,
            (Some(file), None) => (ProfileLayer::File, file, None),
            (Some(file), Some(stored)) => (ProfileLayer::Replaced, stored, Some(ApiProfile::from(file))),
            (None, Some(stored)) => (ProfileLayer::Added, stored, None),
        };
        Some(ProfileEntry { name: name.to_owned(), layer, profile: ApiProfile::from(profile), file })
    }

    pub fn view(&self) -> ProfilesView {
        let names: BTreeSet<&String> = self.file.profiles.keys().chain(self.stored.keys()).collect();
        ProfilesView {
            profiles: names.into_iter().filter_map(|name| self.entry(name)).collect(),
            warnings: self.effective().warnings(),
        }
    }

    pub fn export(&self) -> String {
        #[derive(Serialize)]
        struct Profiles<'a> {
            profiles: BTreeMap<&'a str, &'a ProfileSpec>,
        }
        #[derive(Serialize)]
        struct Fillers<'a> {
            fillers: &'a BTreeMap<String, Vec<String>>,
        }

        let mut out = format!(
            "# profiles.toml with the psst server's profiles, exported by psst-server {}.\n\
             # Use it as the NixOS module's settings.profiles (builtins.fromTOML reads it); once the\n\
             # server runs with it, reset each profile marked below in the web UI.\n",
            env!("CARGO_PKG_VERSION")
        );
        let effective = self.effective();
        for (name, spec) in &effective.profiles {
            out.push('\n');
            match (self.file.profiles.contains_key(name), self.stored.contains_key(name)) {
                (true, true) => out.push_str("# From the server's database, in place of profiles.toml's.\n"),
                (false, true) => out.push_str("# From the server's database; profiles.toml doesn't have it.\n"),
                _ => {}
            }
            let table = Profiles { profiles: [(name.as_str(), spec)].into() };
            out.push_str(&toml::to_string(&table).expect("profiles serialise as TOML"));
        }
        out.push('\n');
        if effective.fillers.is_empty() {
            // An empty table, unlike none, means no fillers in any language.
            out.push_str("[fillers]\n");
        } else {
            out.push_str(&toml::to_string(&Fillers { fillers: &effective.fillers }).expect("fillers serialise"));
        }
        out
    }
}

pub fn change(live: &LiveConfig, db: &Db, name: &str, profile: Option<ApiProfile>) -> Result<ProfileChange, ApiErr> {
    let bad_request = |message: String| ApiErr::new(ApiErrorCode::BadRequest, message);
    let mut layers = live.profile_layers();
    match profile {
        Some(profile) => {
            let spec =
                ProfileSpec::try_from(profile).map_err(|err| bad_request(format!("profiles.{name}.rules: {err}")))?;
            let mut proposed = layers.effective();
            proposed.profiles.insert(name.to_owned(), spec.clone());
            proposed.check().map_err(bad_request)?;
            db.put_profile(name, &ApiProfile::from(&spec))?;
            layers.stored.insert(name.to_owned(), spec);
            tracing::info!("profiles: `{name}` is now the database's version");
        }
        None => {
            if !layers.stored.contains_key(name) {
                return Err(ApiErr::new(
                    ApiErrorCode::NotFound,
                    format!("profile `{name}` has no database version to reset"),
                ));
            }
            db.delete_profile(name)?;
            layers.stored.remove(name);
            tracing::info!("profiles: `{name}` reset");
        }
    }
    live.set_profiles(layers.effective());
    let warnings = match layers.effective().profiles.get(name) {
        Some(spec) => {
            ProfilesFile { profiles: [(name.to_owned(), spec.clone())].into(), fillers: BTreeMap::new() }.warnings()
        }
        None => Vec::new(),
    };
    Ok(ProfileChange { profile: layers.entry(name), warnings })
}
