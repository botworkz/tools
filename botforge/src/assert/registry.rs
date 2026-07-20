use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_yaml::Value;
use std::collections::BTreeMap;

use crate::ssh::SshOptions;

use super::{
    run_assert_files, run_assert_groups, run_assert_packages, run_assert_services,
    run_assert_users, validate_assert_file_entry, validate_assert_group_entry,
    validate_assert_package_entry, validate_assert_service_entry, validate_assert_user_entry,
    AssertBlock, AssertFile, AssertGroup, AssertPackage, AssertService, AssertUser,
};

pub(crate) trait AssertKind: Sync {
    fn verb(&self) -> &'static str;
    fn parse_into(&self, raw_value: &Value, block: &mut AssertBlock) -> Result<()>;
    fn validate(&self, block: &AssertBlock) -> Result<()>;
    fn run(
        &self,
        ssh: &SshOptions,
        block: &AssertBlock,
        installer_username: Option<&str>,
    ) -> Result<()>;
    fn is_empty(&self, block: &AssertBlock) -> bool;
}

pub(crate) struct AssertRegistry {
    ordered: Vec<&'static dyn AssertKind>,
    by_verb: BTreeMap<&'static str, &'static dyn AssertKind>,
}

impl AssertRegistry {
    pub(crate) fn get(&self, verb: &str) -> Option<&'static dyn AssertKind> {
        self.by_verb.get(verb).copied()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &'static dyn AssertKind> + '_ {
        self.ordered.iter().copied()
    }

    pub(crate) fn known_verbs(&self) -> Vec<&'static str> {
        self.ordered.iter().map(|kind| kind.verb()).collect()
    }
}

pub(crate) fn built_in_assert_registry() -> AssertRegistry {
    static FILES: FilesAssertKind = FilesAssertKind;
    static USERS: UsersAssertKind = UsersAssertKind;
    static GROUPS: GroupsAssertKind = GroupsAssertKind;
    static PACKAGES: PackagesAssertKind = PackagesAssertKind;
    static SERVICES: ServicesAssertKind = ServicesAssertKind;

    let ordered: Vec<&'static dyn AssertKind> = vec![&FILES, &USERS, &GROUPS, &PACKAGES, &SERVICES];
    let by_verb = ordered
        .iter()
        .map(|kind| (kind.verb(), *kind))
        .collect::<BTreeMap<_, _>>();

    AssertRegistry { ordered, by_verb }
}

fn parse_entries<T: DeserializeOwned>(
    raw_value: &Value,
    verb: &str,
) -> Result<BTreeMap<String, T>> {
    serde_yaml::from_value(raw_value.clone())
        .with_context(|| format!("invalid assert.{verb} block: expected a mapping"))
}

fn has_service_field(entry: &serde_yaml::Mapping, field: &str) -> bool {
    entry.keys().any(|k| k.as_str() == Some(field))
}

fn validate_service_exists_attributes(raw_value: &Value) -> Result<()> {
    let Some(entries) = raw_value.as_mapping() else {
        return Ok(());
    };

    for (raw_name, raw_entry) in entries {
        let Some(name) = raw_name.as_str() else {
            continue;
        };
        let Some(entry) = raw_entry.as_mapping() else {
            continue;
        };

        let exists_is_false = entry
            .iter()
            .find(|(k, _)| k.as_str() == Some("exists"))
            .and_then(|(_, v)| v.as_bool())
            == Some(false);

        if exists_is_false
            && (has_service_field(entry, "enabled") || has_service_field(entry, "active"))
        {
            anyhow::bail!(
                "assert.services: entry '{name}': enabled/active must not be set when `exists: false`"
            );
        }
    }

    Ok(())
}

#[derive(Debug)]
struct FilesAssertKind;

impl AssertKind for FilesAssertKind {
    fn verb(&self) -> &'static str {
        "files"
    }

    fn parse_into(&self, raw_value: &Value, block: &mut AssertBlock) -> Result<()> {
        block.files = parse_entries::<AssertFile>(raw_value, self.verb())?;
        Ok(())
    }

    fn validate(&self, block: &AssertBlock) -> Result<()> {
        for (guest_path, expectation) in &block.files {
            validate_assert_file_entry(guest_path, expectation)?;
        }
        Ok(())
    }

    fn run(
        &self,
        ssh: &SshOptions,
        block: &AssertBlock,
        _installer_username: Option<&str>,
    ) -> Result<()> {
        run_assert_files(ssh, block)
    }

    fn is_empty(&self, block: &AssertBlock) -> bool {
        block.files.is_empty()
    }
}

#[derive(Debug)]
struct UsersAssertKind;

impl AssertKind for UsersAssertKind {
    fn verb(&self) -> &'static str {
        "users"
    }

    fn parse_into(&self, raw_value: &Value, block: &mut AssertBlock) -> Result<()> {
        block.users = parse_entries::<AssertUser>(raw_value, self.verb())?;
        Ok(())
    }

    fn validate(&self, block: &AssertBlock) -> Result<()> {
        for (name_or_pattern, expectation) in &block.users {
            validate_assert_user_entry(name_or_pattern, expectation)?;
        }
        Ok(())
    }

    fn run(
        &self,
        ssh: &SshOptions,
        block: &AssertBlock,
        installer_username: Option<&str>,
    ) -> Result<()> {
        run_assert_users(ssh, block, installer_username)
    }

    fn is_empty(&self, block: &AssertBlock) -> bool {
        block.users.is_empty()
    }
}

#[derive(Debug)]
struct GroupsAssertKind;

impl AssertKind for GroupsAssertKind {
    fn verb(&self) -> &'static str {
        "groups"
    }

    fn parse_into(&self, raw_value: &Value, block: &mut AssertBlock) -> Result<()> {
        block.groups = parse_entries::<AssertGroup>(raw_value, self.verb())?;
        Ok(())
    }

    fn validate(&self, block: &AssertBlock) -> Result<()> {
        for (name_or_pattern, expectation) in &block.groups {
            validate_assert_group_entry(name_or_pattern, expectation)?;
        }
        Ok(())
    }

    fn run(
        &self,
        ssh: &SshOptions,
        block: &AssertBlock,
        installer_username: Option<&str>,
    ) -> Result<()> {
        run_assert_groups(ssh, block, installer_username)
    }

    fn is_empty(&self, block: &AssertBlock) -> bool {
        block.groups.is_empty()
    }
}

#[derive(Debug)]
struct PackagesAssertKind;

impl AssertKind for PackagesAssertKind {
    fn verb(&self) -> &'static str {
        "packages"
    }

    fn parse_into(&self, raw_value: &Value, block: &mut AssertBlock) -> Result<()> {
        block.packages = parse_entries::<AssertPackage>(raw_value, self.verb())?;
        Ok(())
    }

    fn validate(&self, block: &AssertBlock) -> Result<()> {
        for (name_or_pattern, expectation) in &block.packages {
            validate_assert_package_entry(name_or_pattern, expectation)?;
        }
        Ok(())
    }

    fn run(
        &self,
        ssh: &SshOptions,
        block: &AssertBlock,
        _installer_username: Option<&str>,
    ) -> Result<()> {
        run_assert_packages(ssh, block)
    }

    fn is_empty(&self, block: &AssertBlock) -> bool {
        block.packages.is_empty()
    }
}

#[derive(Debug)]
struct ServicesAssertKind;

impl AssertKind for ServicesAssertKind {
    fn verb(&self) -> &'static str {
        "services"
    }

    fn parse_into(&self, raw_value: &Value, block: &mut AssertBlock) -> Result<()> {
        validate_service_exists_attributes(raw_value)?;
        block.services = parse_entries::<AssertService>(raw_value, self.verb())?;
        Ok(())
    }

    fn validate(&self, block: &AssertBlock) -> Result<()> {
        for (name, expectation) in &block.services {
            validate_assert_service_entry(name, expectation)?;
        }
        Ok(())
    }

    fn run(
        &self,
        ssh: &SshOptions,
        block: &AssertBlock,
        _installer_username: Option<&str>,
    ) -> Result<()> {
        run_assert_services(ssh, block)
    }

    fn is_empty(&self, block: &AssertBlock) -> bool {
        block.services.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::validate_service_exists_attributes;
    use serde_yaml::Value;

    #[test]
    fn test_validate_service_exists_attributes_accepts_exists_false_without_attrs() {
        let raw: Value = serde_yaml::from_str(
            r#"
retired:
  exists: false
"#,
        )
        .unwrap();
        validate_service_exists_attributes(&raw).unwrap();
    }

    #[test]
    fn test_validate_service_exists_attributes_rejects_enabled_with_exists_false() {
        let raw: Value = serde_yaml::from_str(
            r#"
retired:
  exists: false
  enabled: true
"#,
        )
        .unwrap();
        let err = validate_service_exists_attributes(&raw).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exists: false"), "{msg}");
    }
}
