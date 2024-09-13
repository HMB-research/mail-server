/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use ahash::AHashSet;
use jmap_proto::types::collection::Collection;
use store::{
    write::{
        assert::HashedValue, key::DeserializeBigEndian, AssignedIds, BatchBuilder, DirectoryClass,
        MaybeDynamicId, MaybeDynamicValue, SerializeWithId, ValueClass,
    },
    Deserialize, IterateParams, Serialize, Store, ValueKey, U32_LEN,
};
use trc::AddContext;

use crate::{Permission, Principal, QueryBy, Type, ROLE_ADMIN, ROLE_TENANT_ADMIN, ROLE_USER};

use super::{
    lookup::DirectoryStore, PrincipalAction, PrincipalField, PrincipalInfo, PrincipalUpdate,
    PrincipalValue, SpecialSecrets,
};

pub struct MemberOf {
    pub principal_id: u32,
    pub typ: Type,
}

#[allow(async_fn_in_trait)]
pub trait ManageDirectory: Sized {
    async fn get_principal_id(&self, name: &str) -> trc::Result<Option<u32>>;
    async fn get_principal_info(&self, name: &str) -> trc::Result<Option<PrincipalInfo>>;
    async fn get_or_create_principal_id(&self, name: &str, typ: Type) -> trc::Result<u32>;
    async fn get_principal_name(&self, principal_id: u32) -> trc::Result<Option<String>>;
    async fn get_member_of(&self, principal_id: u32) -> trc::Result<Vec<MemberOf>>;
    async fn get_members(&self, principal_id: u32) -> trc::Result<Vec<u32>>;
    async fn create_principal(
        &self,
        principal: Principal,
        tenant_id: Option<u32>,
    ) -> trc::Result<u32>;
    async fn update_principal(
        &self,
        by: QueryBy<'_>,
        changes: Vec<PrincipalUpdate>,
        tenant_id: Option<u32>,
    ) -> trc::Result<()>;
    async fn delete_principal(&self, by: QueryBy<'_>) -> trc::Result<()>;
    async fn list_principals(
        &self,
        filter: Option<&str>,
        typ: Option<Type>,
        tenant_id: Option<u32>,
    ) -> trc::Result<Vec<String>>;
    async fn count_principals(
        &self,
        filter: Option<&str>,
        typ: Option<Type>,
        tenant_id: Option<u32>,
    ) -> trc::Result<u64>;
}

impl ManageDirectory for Store {
    async fn get_principal_name(&self, principal_id: u32) -> trc::Result<Option<String>> {
        self.get_value::<Principal>(ValueKey::from(ValueClass::Directory(
            DirectoryClass::Principal(principal_id),
        )))
        .await
        .map(|v| v.and_then(|mut v| v.take_str(PrincipalField::Name)))
        .caused_by(trc::location!())
    }

    async fn get_principal_id(&self, name: &str) -> trc::Result<Option<u32>> {
        self.get_principal_info(name).await.map(|v| v.map(|v| v.id))
    }
    async fn get_principal_info(&self, name: &str) -> trc::Result<Option<PrincipalInfo>> {
        self.get_value::<PrincipalInfo>(ValueKey::from(ValueClass::Directory(
            DirectoryClass::NameToId(name.as_bytes().to_vec()),
        )))
        .await
        .caused_by(trc::location!())
    }

    // Used by all directories except internal
    async fn get_or_create_principal_id(&self, name: &str, typ: Type) -> trc::Result<u32> {
        let mut try_count = 0;
        let name = name.to_lowercase();

        loop {
            // Try to obtain ID
            if let Some(principal_id) = self
                .get_principal_id(&name)
                .await
                .caused_by(trc::location!())?
            {
                return Ok(principal_id);
            }

            // Write principal ID
            let name_key =
                ValueClass::Directory(DirectoryClass::NameToId(name.as_bytes().to_vec()));
            let mut batch = BatchBuilder::new();
            batch
                .with_account_id(u32::MAX)
                .with_collection(Collection::Principal)
                .assert_value(name_key.clone(), ())
                .create_document()
                .set(name_key, DynamicPrincipalInfo::new(typ, None))
                .set(
                    ValueClass::Directory(DirectoryClass::Principal(MaybeDynamicId::Dynamic(0))),
                    Principal {
                        typ,
                        ..Default::default()
                    }
                    .with_field(PrincipalField::Name, name.to_string()),
                );

            match self
                .write(batch.build())
                .await
                .and_then(|r| r.last_document_id())
            {
                Ok(principal_id) => {
                    return Ok(principal_id);
                }
                Err(err) => {
                    if err.is_assertion_failure() && try_count < 3 {
                        try_count += 1;
                        continue;
                    } else {
                        return Err(err.caused_by(trc::location!()));
                    }
                }
            }
        }
    }

    async fn create_principal(
        &self,
        mut principal: Principal,
        mut tenant_id: Option<u32>,
    ) -> trc::Result<u32> {
        // Make sure the principal has a name
        let name = principal.name().to_lowercase();
        if name.is_empty() {
            return Err(err_missing(PrincipalField::Name));
        }

        // Tenants must provide principal names including a valid domain
        let mut valid_domains = AHashSet::new();
        if tenant_id.is_some() {
            if let Some(domain) = name.split('@').nth(1) {
                if self
                    .get_principal_info(domain)
                    .await
                    .caused_by(trc::location!())?
                    .filter(|v| v.typ == Type::Domain && v.has_tenant_access(tenant_id))
                    .is_some()
                {
                    valid_domains.insert(domain.to_string());
                }
            }

            if valid_domains.is_empty() {
                return Err(error(
                    "Invalid principal name",
                    "Principal name must include a valid domain".into(),
                ));
            }
        }

        // Make sure new name is not taken
        if self
            .get_principal_id(&name)
            .await
            .caused_by(trc::location!())?
            .is_some()
        {
            return Err(err_exists(PrincipalField::Name, name));
        }
        principal.set(PrincipalField::Name, name);

        // Map member names
        let mut members = Vec::new();
        let mut member_of = Vec::new();
        for (field, expected_type) in [
            (PrincipalField::Members, None),
            (PrincipalField::MemberOf, Some(Type::Group)),
            (PrincipalField::Lists, Some(Type::List)),
            (PrincipalField::Roles, Some(Type::Role)),
        ] {
            if let Some(names) = principal.take_str_array(field) {
                let list = if field == PrincipalField::Members {
                    &mut members
                } else {
                    &mut member_of
                };

                for name in names {
                    list.push(
                        self.get_principal_info(&name)
                            .await
                            .caused_by(trc::location!())?
                            .filter(|v| {
                                expected_type.map_or(true, |t| v.typ == t)
                                    && v.has_tenant_access(tenant_id)
                            })
                            .or_else(|| field.map_internal_roles(&name))
                            .ok_or_else(|| not_found(name))?,
                    );
                }
            }
        }

        // Map permissions
        for field in [
            PrincipalField::EnabledPermissions,
            PrincipalField::DisabledPermissions,
        ] {
            if let Some(names) = principal.take_str_array(field) {
                let mut permissions = Vec::with_capacity(names.len());
                for name in names {
                    let permission = Permission::from_name(&name)
                        .ok_or_else(|| {
                            error(
                                format!("Invalid {} value", field.as_str()),
                                format!("Permission {name:?} is invalid").into(),
                            )
                        })?
                        .id() as u64;

                    if !permissions.contains(&permission) {
                        permissions.push(permission);
                    }
                }

                if !permissions.is_empty() {
                    principal.set(field, permissions);
                }
            }
        }

        // Make sure the e-mail is not taken and validate domain
        for email in principal.iter_mut_str(PrincipalField::Emails) {
            *email = email.to_lowercase();
            if self.rcpt(email).await.caused_by(trc::location!())? {
                return Err(err_exists(PrincipalField::Emails, email.to_string()));
            }
            if let Some(domain) = email.split('@').nth(1) {
                if valid_domains.insert(domain.to_string()) {
                    self.get_principal_info(domain)
                        .await
                        .caused_by(trc::location!())?
                        .filter(|v| v.typ == Type::Domain && v.has_tenant_access(tenant_id))
                        .ok_or_else(|| not_found(domain.to_string()))?;
                }
            }
        }

        // Obtain tenant id
        if let Some(tenant_id) = tenant_id {
            principal.set(PrincipalField::Tenant, tenant_id);
        } else if let Some(tenant_name) = principal.take_str(PrincipalField::Tenant) {
            tenant_id = self
                .get_principal_info(&tenant_name)
                .await
                .caused_by(trc::location!())?
                .filter(|v| v.typ == Type::Tenant)
                .ok_or_else(|| not_found(tenant_name.clone()))?
                .id
                .into();
        }

        // Write principal
        let mut batch = BatchBuilder::new();
        let pinfo_name = DynamicPrincipalInfo::new(principal.typ, tenant_id);
        let pinfo_email = DynamicPrincipalInfo::new(principal.typ, None);
        batch
            .with_account_id(u32::MAX)
            .with_collection(Collection::Principal)
            .create_document()
            .assert_value(
                ValueClass::Directory(DirectoryClass::NameToId(
                    principal.name().to_string().into_bytes(),
                )),
                (),
            )
            .set(
                ValueClass::Directory(DirectoryClass::Principal(MaybeDynamicId::Dynamic(0))),
                principal.clone(),
            )
            .set(
                ValueClass::Directory(DirectoryClass::NameToId(
                    principal
                        .take_str(PrincipalField::Name)
                        .unwrap()
                        .into_bytes(),
                )),
                pinfo_name,
            );

        // Write email to id mapping
        if let Some(emails) = principal
            .take(PrincipalField::Emails)
            .map(|v| v.into_str_array())
        {
            for email in emails {
                batch.set(
                    ValueClass::Directory(DirectoryClass::EmailToId(email.into_bytes())),
                    pinfo_email,
                );
            }
        }

        // Write membership
        for member_of in member_of {
            batch.set(
                ValueClass::Directory(DirectoryClass::MemberOf {
                    principal_id: MaybeDynamicId::Dynamic(0),
                    member_of: MaybeDynamicId::Static(member_of.id),
                }),
                vec![member_of.typ as u8],
            );
            batch.set(
                ValueClass::Directory(DirectoryClass::Members {
                    principal_id: MaybeDynamicId::Static(member_of.id),
                    has_member: MaybeDynamicId::Dynamic(0),
                }),
                vec![],
            );
        }
        for member in members {
            batch.set(
                ValueClass::Directory(DirectoryClass::MemberOf {
                    principal_id: MaybeDynamicId::Static(member.id),
                    member_of: MaybeDynamicId::Dynamic(0),
                }),
                vec![member.typ as u8],
            );
            batch.set(
                ValueClass::Directory(DirectoryClass::Members {
                    principal_id: MaybeDynamicId::Dynamic(0),
                    has_member: MaybeDynamicId::Static(member.id),
                }),
                vec![],
            );
        }

        self.write(batch.build())
            .await
            .and_then(|r| r.last_document_id())
    }

    async fn delete_principal(&self, by: QueryBy<'_>) -> trc::Result<()> {
        // Obtain principal
        let principal_id = match by {
            QueryBy::Name(name) => self
                .get_principal_id(name)
                .await
                .caused_by(trc::location!())?
                .ok_or_else(|| not_found(name.to_string()))?,
            QueryBy::Id(principal_id) => principal_id,
            QueryBy::Credentials(_) => unreachable!(),
        };
        let mut principal = self
            .get_value::<Principal>(ValueKey::from(ValueClass::Directory(
                DirectoryClass::Principal(principal_id),
            )))
            .await
            .caused_by(trc::location!())?
            .ok_or_else(|| not_found(principal_id.to_string()))?;

        // Make sure tenant has no data
        let mut batch = BatchBuilder::new();
        match principal.typ {
            Type::Individual | Type::Group => {
                // Update tenant quota
                if let Some(tenant_id) = principal.tenant() {
                    let quota = self
                        .get_counter(DirectoryClass::UsedQuota(principal_id))
                        .await
                        .caused_by(trc::location!())?;
                    if quota > 0 {
                        batch.add(DirectoryClass::UsedQuota(tenant_id), -quota);
                    }
                }
            }
            Type::Tenant => {
                let tenant_members = self
                    .list_principals(None, None, principal.id().into())
                    .await
                    .caused_by(trc::location!())?;

                if !tenant_members.is_empty() {
                    let tenant_members = if tenant_members.len() > 5 {
                        tenant_members[..5].join(", ")
                            + " and "
                            + &(&tenant_members.len() - 5).to_string()
                            + " others"
                    } else {
                        tenant_members.join(", ")
                    };

                    return Err(error(
                        "Tenant has members",
                        format!(
                            "Tenant must have no members to be deleted: Found: {tenant_members}"
                        )
                        .into(),
                    ));
                }
            }

            _ => {}
        }

        // Unlink all principal's blobs
        self.blob_hash_unlink_account(principal_id)
            .await
            .caused_by(trc::location!())?;

        // Revoke ACLs
        self.acl_revoke_all(principal_id)
            .await
            .caused_by(trc::location!())?;

        // Delete principal data
        self.purge_account(principal_id)
            .await
            .caused_by(trc::location!())?;

        // Delete principal
        batch
            .with_account_id(principal_id)
            .clear(DirectoryClass::NameToId(
                principal
                    .take_str(PrincipalField::Name)
                    .unwrap_or_default()
                    .into_bytes(),
            ))
            .clear(DirectoryClass::Principal(MaybeDynamicId::Static(
                principal_id,
            )))
            .clear(DirectoryClass::UsedQuota(principal_id));

        if let Some(emails) = principal.take_str_array(PrincipalField::Emails) {
            for email in emails {
                batch.clear(DirectoryClass::EmailToId(email.into_bytes()));
            }
        }

        for member in self
            .get_member_of(principal_id)
            .await
            .caused_by(trc::location!())?
        {
            batch.clear(DirectoryClass::MemberOf {
                principal_id: MaybeDynamicId::Static(principal_id),
                member_of: MaybeDynamicId::Static(member.principal_id),
            });
            batch.clear(DirectoryClass::Members {
                principal_id: MaybeDynamicId::Static(member.principal_id),
                has_member: MaybeDynamicId::Static(principal_id),
            });
        }

        for member_id in self
            .get_members(principal_id)
            .await
            .caused_by(trc::location!())?
        {
            batch.clear(DirectoryClass::MemberOf {
                principal_id: MaybeDynamicId::Static(member_id),
                member_of: MaybeDynamicId::Static(principal_id),
            });
            batch.clear(DirectoryClass::Members {
                principal_id: MaybeDynamicId::Static(principal_id),
                has_member: MaybeDynamicId::Static(member_id),
            });
        }

        self.write(batch.build())
            .await
            .caused_by(trc::location!())?;

        Ok(())
    }

    async fn update_principal(
        &self,
        by: QueryBy<'_>,
        changes: Vec<PrincipalUpdate>,
        tenant_id: Option<u32>,
    ) -> trc::Result<()> {
        let principal_id = match by {
            QueryBy::Name(name) => self
                .get_principal_id(name)
                .await
                .caused_by(trc::location!())?
                .ok_or_else(|| not_found(name.to_string()))?,
            QueryBy::Id(principal_id) => principal_id,
            QueryBy::Credentials(_) => unreachable!(),
        };

        // Fetch principal
        let mut principal = self
            .get_value::<HashedValue<Principal>>(ValueKey::from(ValueClass::Directory(
                DirectoryClass::Principal(principal_id),
            )))
            .await
            .caused_by(trc::location!())?
            .ok_or_else(|| not_found(principal_id.to_string()))?;

        // Obtain members and memberOf
        let mut member_of = self
            .get_member_of(principal_id)
            .await
            .caused_by(trc::location!())?
            .into_iter()
            .map(|v| v.principal_id)
            .collect::<Vec<_>>();
        let mut members = self
            .get_members(principal_id)
            .await
            .caused_by(trc::location!())?;

        // Prepare changes
        let mut batch = BatchBuilder::new();
        let mut pinfo_name =
            PrincipalInfo::new(principal_id, principal.inner.typ, principal.inner.tenant())
                .serialize();
        let pinfo_email = PrincipalInfo::new(principal_id, principal.inner.typ, None).serialize();
        let update_principal = !changes.is_empty()
            && !changes.iter().all(|c| {
                matches!(
                    c.field,
                    PrincipalField::MemberOf
                        | PrincipalField::Members
                        | PrincipalField::Lists
                        | PrincipalField::Roles
                )
            });

        if update_principal {
            batch.assert_value(
                ValueClass::Directory(DirectoryClass::Principal(MaybeDynamicId::Static(
                    principal_id,
                ))),
                &principal,
            );
        }

        // Obtain used quota
        let mut used_quota = None;
        if tenant_id.is_none()
            && changes
                .iter()
                .any(|c| matches!(c.field, PrincipalField::Tenant))
        {
            let quota = self
                .get_counter(DirectoryClass::UsedQuota(principal_id))
                .await
                .caused_by(trc::location!())?;
            if quota > 0 {
                used_quota = Some(quota);
            }
        }

        // Allowed principal types for Member fields
        let allowed_member_types = match principal.inner.typ() {
            Type::Group => &[Type::Individual, Type::Group][..],
            Type::Resource => &[Type::Resource][..],
            Type::Location => &[
                Type::Location,
                Type::Resource,
                Type::Individual,
                Type::Group,
                Type::Other,
            ][..],
            Type::List => &[Type::Individual, Type::Group][..],
            Type::Other | Type::Domain | Type::Tenant | Type::Individual => &[][..],
            Type::Role => &[Type::Role][..],
        };
        let mut valid_domains = AHashSet::new();

        // Process changes
        for change in changes {
            match (change.action, change.field, change.value) {
                (PrincipalAction::Set, PrincipalField::Name, PrincipalValue::String(new_name)) => {
                    // Make sure new name is not taken
                    let new_name = new_name.to_lowercase();
                    if principal.inner.name() != new_name {
                        if tenant_id.is_some() {
                            if let Some(domain) = new_name.split('@').nth(1) {
                                if self
                                    .get_principal_info(domain)
                                    .await
                                    .caused_by(trc::location!())?
                                    .filter(|v| {
                                        v.typ == Type::Domain && v.has_tenant_access(tenant_id)
                                    })
                                    .is_some()
                                {
                                    valid_domains.insert(domain.to_string());
                                }
                            }

                            if valid_domains.is_empty() {
                                return Err(error(
                                    "Invalid principal name",
                                    "Principal name must include a valid domain".into(),
                                ));
                            }
                        }

                        if self
                            .get_principal_id(&new_name)
                            .await
                            .caused_by(trc::location!())?
                            .is_some()
                        {
                            return Err(err_exists(PrincipalField::Name, new_name));
                        }

                        batch.clear(ValueClass::Directory(DirectoryClass::NameToId(
                            principal.inner.name().as_bytes().to_vec(),
                        )));

                        principal.inner.set(PrincipalField::Name, new_name.clone());

                        batch.set(
                            ValueClass::Directory(DirectoryClass::NameToId(new_name.into_bytes())),
                            pinfo_name.clone(),
                        );
                    }
                }
                (
                    PrincipalAction::Set,
                    PrincipalField::Tenant,
                    PrincipalValue::String(tenant_name),
                ) if tenant_id.is_none() => {
                    if !tenant_name.is_empty() {
                        let tenant_info = self
                            .get_principal_info(&tenant_name)
                            .await
                            .caused_by(trc::location!())?
                            .ok_or_else(|| not_found(tenant_name.clone()))?;

                        if tenant_info.typ != Type::Tenant {
                            return Err(error(
                                "Not a tenant",
                                format!("Principal {tenant_name:?} is not a tenant").into(),
                            ));
                        }

                        match principal.inner.tenant() {
                            Some(old_tenant_id) if old_tenant_id != tenant_info.id => {
                                // Update quota
                                if let Some(used_quota) = used_quota {
                                    batch
                                        .add(DirectoryClass::UsedQuota(old_tenant_id), -used_quota)
                                        .add(DirectoryClass::UsedQuota(tenant_info.id), used_quota);
                                }

                                principal.inner.set(PrincipalField::Tenant, tenant_info.id);
                                pinfo_name = PrincipalInfo::new(
                                    principal_id,
                                    principal.inner.typ,
                                    tenant_info.id.into(),
                                )
                                .serialize();
                            }
                            _ => continue,
                        }
                    } else if let Some(tenant_id) = principal.inner.tenant() {
                        // Update quota
                        if let Some(used_quota) = used_quota {
                            batch.add(DirectoryClass::UsedQuota(tenant_id), -used_quota);
                        }

                        principal.inner.remove(PrincipalField::Tenant);
                        pinfo_name =
                            PrincipalInfo::new(principal_id, principal.inner.typ, None).serialize();
                    } else {
                        continue;
                    }

                    batch.set(
                        ValueClass::Directory(DirectoryClass::NameToId(
                            principal.inner.name().as_bytes().to_vec(),
                        )),
                        pinfo_name.clone(),
                    );
                }
                (
                    PrincipalAction::Set,
                    PrincipalField::Secrets,
                    value @ (PrincipalValue::StringList(_) | PrincipalValue::String(_)),
                ) => {
                    principal.inner.set(PrincipalField::Secrets, value);
                }
                (
                    PrincipalAction::AddItem,
                    PrincipalField::Secrets,
                    PrincipalValue::String(secret),
                ) => {
                    if !principal
                        .inner
                        .has_str_value(PrincipalField::Secrets, &secret)
                    {
                        if secret.is_otp_auth() {
                            // Add OTP Auth URLs to the beginning of the list
                            principal.inner.prepend_str(PrincipalField::Secrets, secret);
                        } else {
                            principal.inner.append_str(PrincipalField::Secrets, secret);
                        }
                    }
                }
                (
                    PrincipalAction::RemoveItem,
                    PrincipalField::Secrets,
                    PrincipalValue::String(secret),
                ) => {
                    if secret.is_app_password() || secret.is_otp_auth() {
                        principal.inner.retain_str(PrincipalField::Secrets, |v| {
                            *v != secret && !v.starts_with(&secret)
                        });
                    } else if !secret.is_empty() {
                        principal
                            .inner
                            .retain_str(PrincipalField::Secrets, |v| *v != secret);
                    } else {
                        principal
                            .inner
                            .retain_str(PrincipalField::Secrets, |v| !v.is_password());
                    }
                }
                (
                    PrincipalAction::Set,
                    PrincipalField::Description,
                    PrincipalValue::String(description),
                ) => {
                    if !description.is_empty() {
                        principal
                            .inner
                            .set(PrincipalField::Description, description);
                    } else {
                        principal.inner.remove(PrincipalField::Description);
                    }
                }
                (PrincipalAction::Set, PrincipalField::Quota, PrincipalValue::Integer(quota))
                    if matches!(
                        principal.inner.typ,
                        Type::Individual | Type::Group | Type::Tenant
                    ) =>
                {
                    principal.inner.set(PrincipalField::Quota, quota);
                }
                (PrincipalAction::Set, PrincipalField::Quota, PrincipalValue::String(quota))
                    if matches!(
                        principal.inner.typ,
                        Type::Individual | Type::Group | Type::Tenant
                    ) && quota.is_empty() =>
                {
                    principal.inner.remove(PrincipalField::Quota);
                }
                (
                    PrincipalAction::Set,
                    PrincipalField::Quota,
                    PrincipalValue::IntegerList(quotas),
                ) if matches!(principal.inner.typ, Type::Tenant)
                    && quotas.len() <= (Type::Other as usize + 1) =>
                {
                    principal.inner.set(PrincipalField::Quota, quotas);
                }

                // Emails
                (
                    PrincipalAction::Set,
                    PrincipalField::Emails,
                    PrincipalValue::StringList(emails),
                ) => {
                    // Validate unique emails
                    let emails = emails
                        .into_iter()
                        .map(|v| v.to_lowercase())
                        .collect::<Vec<_>>();
                    for email in &emails {
                        if !principal.inner.has_str_value(PrincipalField::Emails, email) {
                            if self.rcpt(email).await.caused_by(trc::location!())? {
                                return Err(err_exists(PrincipalField::Emails, email.to_string()));
                            }
                            if let Some(domain) = email.split('@').nth(1) {
                                if !self
                                    .is_local_domain(domain)
                                    .await
                                    .caused_by(trc::location!())?
                                {
                                    return Err(not_found(domain.to_string()));
                                }
                            }
                            batch.set(
                                ValueClass::Directory(DirectoryClass::EmailToId(
                                    email.as_bytes().to_vec(),
                                )),
                                pinfo_email.clone(),
                            );
                        }
                    }

                    for email in principal.inner.iter_str(PrincipalField::Emails) {
                        if !emails.contains(email) {
                            batch.clear(ValueClass::Directory(DirectoryClass::EmailToId(
                                email.as_bytes().to_vec(),
                            )));
                        }
                    }

                    principal.inner.set(PrincipalField::Emails, emails);
                }
                (
                    PrincipalAction::AddItem,
                    PrincipalField::Emails,
                    PrincipalValue::String(email),
                ) => {
                    let email = email.to_lowercase();
                    if !principal
                        .inner
                        .has_str_value(PrincipalField::Emails, &email)
                    {
                        if self.rcpt(&email).await.caused_by(trc::location!())? {
                            return Err(err_exists(PrincipalField::Emails, email));
                        }
                        if let Some(domain) = email.split('@').nth(1) {
                            if !self
                                .is_local_domain(domain)
                                .await
                                .caused_by(trc::location!())?
                            {
                                return Err(not_found(domain.to_string()));
                            }
                        }
                        batch.set(
                            ValueClass::Directory(DirectoryClass::EmailToId(
                                email.as_bytes().to_vec(),
                            )),
                            pinfo_email.clone(),
                        );
                        principal.inner.append_str(PrincipalField::Emails, email);
                    }
                }
                (
                    PrincipalAction::RemoveItem,
                    PrincipalField::Emails,
                    PrincipalValue::String(email),
                ) => {
                    let email = email.to_lowercase();
                    if principal
                        .inner
                        .has_str_value(PrincipalField::Emails, &email)
                    {
                        principal
                            .inner
                            .retain_str(PrincipalField::Emails, |v| *v != email);
                        batch.clear(ValueClass::Directory(DirectoryClass::EmailToId(
                            email.into_bytes(),
                        )));
                    }
                }

                // MemberOf
                (
                    PrincipalAction::Set,
                    PrincipalField::MemberOf | PrincipalField::Lists | PrincipalField::Roles,
                    PrincipalValue::StringList(members),
                ) => {
                    let mut new_member_of = Vec::new();
                    for member in members {
                        let member_info = self
                            .get_principal_info(&member)
                            .await
                            .caused_by(trc::location!())?
                            .filter(|p| p.has_tenant_access(tenant_id))
                            .or_else(|| change.field.map_internal_roles(&member))
                            .ok_or_else(|| not_found(member.clone()))?;

                        let expected_type = match change.field {
                            PrincipalField::MemberOf => Type::Group,
                            PrincipalField::Lists => Type::List,
                            PrincipalField::Roles => Type::Role,
                            _ => unreachable!(),
                        };

                        if member_info.typ != expected_type {
                            return Err(error(
                                format!("Invalid {} value", change.field.as_str()),
                                format!(
                                    "Principal {member:?} is not a {}.",
                                    expected_type.as_str()
                                )
                                .into(),
                            ));
                        }

                        if !member_of.contains(&member_info.id) {
                            batch.set(
                                ValueClass::Directory(DirectoryClass::MemberOf {
                                    principal_id: MaybeDynamicId::Static(principal_id),
                                    member_of: MaybeDynamicId::Static(member_info.id),
                                }),
                                vec![member_info.typ as u8],
                            );
                            batch.set(
                                ValueClass::Directory(DirectoryClass::Members {
                                    principal_id: MaybeDynamicId::Static(member_info.id),
                                    has_member: MaybeDynamicId::Static(principal_id),
                                }),
                                vec![],
                            );
                        }

                        new_member_of.push(member_info.id);
                    }

                    for member_id in &member_of {
                        if !new_member_of.contains(member_id) {
                            batch.clear(ValueClass::Directory(DirectoryClass::MemberOf {
                                principal_id: MaybeDynamicId::Static(principal_id),
                                member_of: MaybeDynamicId::Static(*member_id),
                            }));
                            batch.clear(ValueClass::Directory(DirectoryClass::Members {
                                principal_id: MaybeDynamicId::Static(*member_id),
                                has_member: MaybeDynamicId::Static(principal_id),
                            }));
                        }
                    }

                    member_of = new_member_of;
                }
                (
                    PrincipalAction::AddItem,
                    PrincipalField::MemberOf | PrincipalField::Lists | PrincipalField::Roles,
                    PrincipalValue::String(member),
                ) => {
                    let member_info = self
                        .get_principal_info(&member)
                        .await
                        .caused_by(trc::location!())?
                        .filter(|p| p.has_tenant_access(tenant_id))
                        .or_else(|| change.field.map_internal_roles(&member))
                        .ok_or_else(|| not_found(member.clone()))?;

                    if !member_of.contains(&member_info.id) {
                        let expected_type = match change.field {
                            PrincipalField::MemberOf => Type::Group,
                            PrincipalField::Lists => Type::List,
                            PrincipalField::Roles => Type::Role,
                            _ => unreachable!(),
                        };

                        if member_info.typ != expected_type {
                            return Err(error(
                                format!("Invalid {} value", change.field.as_str()),
                                format!(
                                    "Principal {member:?} is not a {}.",
                                    expected_type.as_str()
                                )
                                .into(),
                            ));
                        }

                        batch.set(
                            ValueClass::Directory(DirectoryClass::MemberOf {
                                principal_id: MaybeDynamicId::Static(principal_id),
                                member_of: MaybeDynamicId::Static(member_info.id),
                            }),
                            vec![member_info.typ as u8],
                        );

                        batch.set(
                            ValueClass::Directory(DirectoryClass::Members {
                                principal_id: MaybeDynamicId::Static(member_info.id),
                                has_member: MaybeDynamicId::Static(principal_id),
                            }),
                            vec![],
                        );

                        member_of.push(member_info.id);
                    }
                }
                (
                    PrincipalAction::RemoveItem,
                    PrincipalField::MemberOf | PrincipalField::Lists | PrincipalField::Roles,
                    PrincipalValue::String(member),
                ) => {
                    if let Some(member_id) = self
                        .get_principal_id(&member)
                        .await
                        .caused_by(trc::location!())?
                        .or_else(|| change.field.map_internal_role_name(&member))
                    {
                        if let Some(pos) = member_of.iter().position(|v| *v == member_id) {
                            batch.clear(ValueClass::Directory(DirectoryClass::MemberOf {
                                principal_id: MaybeDynamicId::Static(principal_id),
                                member_of: MaybeDynamicId::Static(member_id),
                            }));

                            batch.clear(ValueClass::Directory(DirectoryClass::Members {
                                principal_id: MaybeDynamicId::Static(member_id),
                                has_member: MaybeDynamicId::Static(principal_id),
                            }));

                            member_of.remove(pos);
                        }
                    }
                }

                (
                    PrincipalAction::Set,
                    PrincipalField::Members,
                    PrincipalValue::StringList(members_),
                ) => {
                    let mut new_members = Vec::new();

                    for member in members_ {
                        let member_info = self
                            .get_principal_info(&member)
                            .await
                            .caused_by(trc::location!())?
                            .filter(|p| p.has_tenant_access(tenant_id))
                            .ok_or_else(|| not_found(member.clone()))?;

                        if !allowed_member_types.contains(&member_info.typ) {
                            return Err(error(
                                "Invalid members value",
                                format!(
                                    "Principal {member:?} is not one of {}.",
                                    allowed_member_types
                                        .iter()
                                        .map(|v| v.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                                .into(),
                            ));
                        }

                        if !members.contains(&member_info.id) {
                            batch.set(
                                ValueClass::Directory(DirectoryClass::MemberOf {
                                    principal_id: MaybeDynamicId::Static(member_info.id),
                                    member_of: MaybeDynamicId::Static(principal_id),
                                }),
                                vec![member_info.typ as u8],
                            );
                            batch.set(
                                ValueClass::Directory(DirectoryClass::Members {
                                    principal_id: MaybeDynamicId::Static(principal_id),
                                    has_member: MaybeDynamicId::Static(member_info.id),
                                }),
                                vec![],
                            );
                        }

                        new_members.push(member_info.id);
                    }

                    for member_id in &members {
                        if !new_members.contains(member_id) {
                            batch.clear(ValueClass::Directory(DirectoryClass::MemberOf {
                                principal_id: MaybeDynamicId::Static(*member_id),
                                member_of: MaybeDynamicId::Static(principal_id),
                            }));
                            batch.clear(ValueClass::Directory(DirectoryClass::Members {
                                principal_id: MaybeDynamicId::Static(principal_id),
                                has_member: MaybeDynamicId::Static(*member_id),
                            }));
                        }
                    }

                    members = new_members;
                }
                (
                    PrincipalAction::AddItem,
                    PrincipalField::Members,
                    PrincipalValue::String(member),
                ) => {
                    let member_info = self
                        .get_principal_info(&member)
                        .await
                        .caused_by(trc::location!())?
                        .filter(|p| p.has_tenant_access(tenant_id))
                        .ok_or_else(|| not_found(member.clone()))?;

                    if !members.contains(&member_info.id) {
                        if !allowed_member_types.contains(&member_info.typ) {
                            return Err(error(
                                "Invalid members value",
                                format!(
                                    "Principal {member:?} is not one of {}.",
                                    allowed_member_types
                                        .iter()
                                        .map(|v| v.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                                .into(),
                            ));
                        }

                        batch.set(
                            ValueClass::Directory(DirectoryClass::MemberOf {
                                principal_id: MaybeDynamicId::Static(member_info.id),
                                member_of: MaybeDynamicId::Static(principal_id),
                            }),
                            vec![member_info.typ as u8],
                        );
                        batch.set(
                            ValueClass::Directory(DirectoryClass::Members {
                                principal_id: MaybeDynamicId::Static(principal_id),
                                has_member: MaybeDynamicId::Static(member_info.id),
                            }),
                            vec![],
                        );
                        members.push(member_info.id);
                    }
                }
                (
                    PrincipalAction::RemoveItem,
                    PrincipalField::Members,
                    PrincipalValue::String(member),
                ) => {
                    if let Some(member_id) = self
                        .get_principal_id(&member)
                        .await
                        .caused_by(trc::location!())?
                    {
                        if let Some(pos) = members.iter().position(|v| *v == member_id) {
                            batch.clear(ValueClass::Directory(DirectoryClass::MemberOf {
                                principal_id: MaybeDynamicId::Static(member_id),
                                member_of: MaybeDynamicId::Static(principal_id),
                            }));
                            batch.clear(ValueClass::Directory(DirectoryClass::Members {
                                principal_id: MaybeDynamicId::Static(principal_id),
                                has_member: MaybeDynamicId::Static(member_id),
                            }));
                            members.remove(pos);
                        }
                    }
                }

                (
                    PrincipalAction::Set,
                    PrincipalField::EnabledPermissions | PrincipalField::DisabledPermissions,
                    PrincipalValue::StringList(names),
                ) => {
                    let mut permissions = Vec::with_capacity(names.len());
                    for name in names {
                        let permission = Permission::from_name(&name)
                            .ok_or_else(|| {
                                error(
                                    format!("Invalid {} value", change.field.as_str()),
                                    format!("Permission {name:?} is invalid").into(),
                                )
                            })?
                            .id() as u64;

                        if !permissions.contains(&permission) {
                            permissions.push(permission);
                        }
                    }

                    if !permissions.is_empty() {
                        principal.inner.set(change.field, permissions);
                    } else {
                        principal.inner.remove(change.field);
                    }
                }
                (
                    PrincipalAction::AddItem,
                    PrincipalField::EnabledPermissions | PrincipalField::DisabledPermissions,
                    PrincipalValue::String(name),
                ) => {
                    let permission = Permission::from_name(&name)
                        .ok_or_else(|| {
                            error(
                                format!("Invalid {} value", change.field.as_str()),
                                format!("Permission {name:?} is invalid").into(),
                            )
                        })?
                        .id() as u64;

                    principal.inner.append_int(change.field, permission);
                }
                (
                    PrincipalAction::RemoveItem,
                    PrincipalField::EnabledPermissions | PrincipalField::DisabledPermissions,
                    PrincipalValue::String(name),
                ) => {
                    let permission = Permission::from_name(&name)
                        .ok_or_else(|| {
                            error(
                                format!("Invalid {} value", change.field.as_str()),
                                format!("Permission {name:?} is invalid").into(),
                            )
                        })?
                        .id() as u64;

                    principal
                        .inner
                        .retain_int(change.field, |v| *v != permission);
                }

                _ => {
                    return Err(trc::StoreEvent::NotSupported.caused_by(trc::location!()));
                }
            }
        }

        if update_principal {
            batch.set(
                ValueClass::Directory(DirectoryClass::Principal(MaybeDynamicId::Static(
                    principal_id,
                ))),
                principal.inner.serialize(),
            );
        }

        self.write(batch.build())
            .await
            .caused_by(trc::location!())?;

        Ok(())
    }

    async fn list_principals(
        &self,
        filter: Option<&str>,
        typ: Option<Type>,
        tenant_id: Option<u32>,
    ) -> trc::Result<Vec<String>> {
        let from_key = ValueKey::from(ValueClass::Directory(DirectoryClass::NameToId(vec![])));
        let to_key = ValueKey::from(ValueClass::Directory(DirectoryClass::NameToId(vec![
            u8::MAX;
            10
        ])));

        let mut results = Vec::new();
        self.iterate(
            IterateParams::new(from_key, to_key).ascending(),
            |key, value| {
                let pt = PrincipalInfo::deserialize(value).caused_by(trc::location!())?;

                if typ.map_or(true, |t| pt.typ == t) && pt.has_tenant_access(tenant_id) {
                    results.push((
                        pt.id,
                        String::from_utf8_lossy(key.get(1..).unwrap_or_default()).into_owned(),
                    ));
                }

                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;

        if let Some(filter) = filter {
            let mut filtered = Vec::new();
            let filters = filter
                .split_whitespace()
                .map(|r| r.to_lowercase())
                .collect::<Vec<_>>();

            for (principal_id, principal_name) in results {
                let principal = self
                    .get_value::<Principal>(ValueKey::from(ValueClass::Directory(
                        DirectoryClass::Principal(principal_id),
                    )))
                    .await
                    .caused_by(trc::location!())?
                    .ok_or_else(|| not_found(principal_id.to_string()))?;
                if filters.iter().all(|f| principal.find_str(f)) {
                    filtered.push(principal_name);
                }
            }

            Ok(filtered)
        } else {
            Ok(results.into_iter().map(|(_, name)| name).collect())
        }
    }

    async fn count_principals(
        &self,
        filter: Option<&str>,
        typ: Option<Type>,
        tenant_id: Option<u32>,
    ) -> trc::Result<u64> {
        let from_key = ValueKey::from(ValueClass::Directory(DirectoryClass::NameToId(vec![])));
        let to_key = ValueKey::from(ValueClass::Directory(DirectoryClass::NameToId(vec![
            u8::MAX;
            10
        ])));

        let mut count = 0;
        self.iterate(
            IterateParams::new(from_key, to_key).ascending(),
            |key, value| {
                let pt = PrincipalInfo::deserialize(value).caused_by(trc::location!())?;
                let name =
                    std::str::from_utf8(key.get(1..).unwrap_or_default()).unwrap_or_default();

                if typ.map_or(true, |t| pt.typ == t)
                    && pt.has_tenant_access(tenant_id)
                    && filter.map_or(true, |f| name.contains(f))
                {
                    count += 1;
                }

                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())
        .map(|_| count)
    }

    async fn get_member_of(&self, principal_id: u32) -> trc::Result<Vec<MemberOf>> {
        let from_key = ValueKey::from(ValueClass::Directory(DirectoryClass::MemberOf {
            principal_id,
            member_of: 0,
        }));
        let to_key = ValueKey::from(ValueClass::Directory(DirectoryClass::MemberOf {
            principal_id,
            member_of: u32::MAX,
        }));
        let mut results = Vec::new();
        self.iterate(IterateParams::new(from_key, to_key), |key, value| {
            results.push(MemberOf {
                principal_id: key.deserialize_be_u32(key.len() - U32_LEN)?,
                typ: value
                    .first()
                    .map(|v| Type::from_u8(*v))
                    .unwrap_or(Type::Group),
            });
            Ok(true)
        })
        .await
        .caused_by(trc::location!())?;
        Ok(results)
    }

    async fn get_members(&self, principal_id: u32) -> trc::Result<Vec<u32>> {
        let from_key = ValueKey::from(ValueClass::Directory(DirectoryClass::Members {
            principal_id,
            has_member: 0,
        }));
        let to_key = ValueKey::from(ValueClass::Directory(DirectoryClass::Members {
            principal_id,
            has_member: u32::MAX,
        }));
        let mut results = Vec::new();
        self.iterate(
            IterateParams::new(from_key, to_key).no_values(),
            |key, _| {
                results.push(key.deserialize_be_u32(key.len() - U32_LEN)?);
                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;
        Ok(results)
    }
}

impl PrincipalField {
    pub fn map_internal_role_name(&self, name: &str) -> Option<u32> {
        match (self, name) {
            (PrincipalField::Roles, "admin") => Some(ROLE_ADMIN),
            (PrincipalField::Roles, "tenant-admin") => Some(ROLE_TENANT_ADMIN),
            (PrincipalField::Roles, "user") => Some(ROLE_USER),
            _ => None,
        }
    }

    pub fn map_internal_roles(&self, name: &str) -> Option<PrincipalInfo> {
        self.map_internal_role_name(name)
            .map(|role_id| PrincipalInfo::new(role_id, Type::Role, None))
    }
}

impl SerializeWithId for Principal {
    fn serialize_with_id(&self, ids: &AssignedIds) -> trc::Result<Vec<u8>> {
        let mut principal = self.clone();
        principal.id = ids.last_document_id().caused_by(trc::location!())?;
        Ok(principal.serialize())
    }
}

impl From<Principal> for MaybeDynamicValue {
    fn from(principal: Principal) -> Self {
        MaybeDynamicValue::Dynamic(Box::new(principal))
    }
}

#[derive(Clone, Copy)]
struct DynamicPrincipalInfo {
    typ: Type,
    tenant: Option<u32>,
}

impl DynamicPrincipalInfo {
    fn new(typ: Type, tenant: Option<u32>) -> Self {
        Self { typ, tenant }
    }
}

impl SerializeWithId for DynamicPrincipalInfo {
    fn serialize_with_id(&self, ids: &AssignedIds) -> trc::Result<Vec<u8>> {
        ids.last_document_id()
            .map(|principal_id| PrincipalInfo::new(principal_id, self.typ, self.tenant).serialize())
    }
}

impl From<DynamicPrincipalInfo> for MaybeDynamicValue {
    fn from(value: DynamicPrincipalInfo) -> Self {
        MaybeDynamicValue::Dynamic(Box::new(value))
    }
}

pub fn err_missing(field: impl Into<trc::Value>) -> trc::Error {
    trc::ManageEvent::MissingParameter.ctx(trc::Key::Key, field)
}

pub fn err_exists(field: impl Into<trc::Value>, value: impl Into<trc::Value>) -> trc::Error {
    trc::ManageEvent::AlreadyExists
        .ctx(trc::Key::Key, field)
        .ctx(trc::Key::Value, value)
}

pub fn not_found(value: impl Into<trc::Value>) -> trc::Error {
    trc::ManageEvent::NotFound.ctx(trc::Key::Key, value)
}

pub fn unsupported(details: impl Into<trc::Value>) -> trc::Error {
    trc::ManageEvent::NotSupported.ctx(trc::Key::Details, details)
}

pub fn enterprise() -> trc::Error {
    trc::ManageEvent::NotSupported.ctx(trc::Key::Details, "Enterprise feature")
}

pub fn error(details: impl Into<trc::Value>, reason: Option<impl Into<trc::Value>>) -> trc::Error {
    trc::ManageEvent::Error
        .ctx(trc::Key::Details, details)
        .ctx_opt(trc::Key::Reason, reason)
}

impl From<PrincipalField> for trc::Value {
    fn from(value: PrincipalField) -> Self {
        trc::Value::Static(value.as_str())
    }
}
