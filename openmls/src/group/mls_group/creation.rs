use openmls_traits::{signatures::Signer, storage::StorageProvider as StorageProviderTrait};

use super::{builder::MlsGroupBuilder, *};
use crate::{
    credentials::CredentialWithKey,
    group::{
        core_group::create_commit_params::{CommitType, CreateCommitParams},
        errors::{ExternalCommitError, WelcomeError},
    },
    messages::{
        group_info::{GroupInfo, VerifiableGroupInfo},
        Welcome,
    },
    schedule::psk::{store::ResumptionPskStore, PreSharedKeyId},
    storage::OpenMlsProvider,
    treesync::{
        node::leaf_node::{Capabilities, LeafNodeParameters},
        RatchetTreeIn,
    },
};

impl MlsGroup {
    // === Group creation ===

    /// Creates a builder which can be used to configure and build
    /// a new [`MlsGroup`].
    pub fn builder() -> MlsGroupBuilder {
        MlsGroupBuilder::new()
    }

    /// Creates a new group with the creator as the only member (and a random
    /// group ID).
    ///
    /// This function removes the private key corresponding to the
    /// `key_package` from the key store.
    pub fn new<Provider: OpenMlsProvider>(
        provider: &Provider,
        signer: &impl Signer,
        mls_group_create_config: &MlsGroupCreateConfig,
        credential_with_key: CredentialWithKey,
    ) -> Result<Self, NewGroupError<Provider::StorageError>> {
        MlsGroupBuilder::new().build_internal(
            provider,
            signer,
            credential_with_key,
            Some(mls_group_create_config.clone()),
        )
    }

    /// Creates a new group with a given group ID with the creator as the only
    /// member.
    pub fn new_with_group_id<Provider: OpenMlsProvider>(
        provider: &Provider,
        signer: &impl Signer,
        mls_group_create_config: &MlsGroupCreateConfig,
        group_id: GroupId,
        credential_with_key: CredentialWithKey,
    ) -> Result<Self, NewGroupError<Provider::StorageError>> {
        MlsGroupBuilder::new()
            .with_group_id(group_id)
            .build_internal(
                provider,
                signer,
                credential_with_key,
                Some(mls_group_create_config.clone()),
            )
    }

    /// Join an existing group through an External Commit.
    /// The resulting [`MlsGroup`] instance starts off with a pending
    /// commit (the external commit, which adds this client to the group).
    /// Merging this commit is necessary for this [`MlsGroup`] instance to
    /// function properly, as, for example, this client is not yet part of the
    /// tree. As a result, it is not possible to clear the pending commit. If
    /// the external commit was rejected due to an epoch change, the
    /// [`MlsGroup`] instance has to be discarded and a new one has to be
    /// created using this function based on the latest `ratchet_tree` and
    /// group info. For more information on the external init process,
    /// please see Section 11.2.1 in the MLS specification.
    ///
    /// Note: If there is a group member in the group with the same identity as
    /// us, this will create a remove proposal.
    #[allow(clippy::too_many_arguments)]
    pub fn join_by_external_commit<Provider: OpenMlsProvider>(
        provider: &Provider,
        signer: &impl Signer,
        ratchet_tree: Option<RatchetTreeIn>,
        verifiable_group_info: VerifiableGroupInfo,
        mls_group_config: &MlsGroupJoinConfig,
        capabilities: Option<Capabilities>,
        extensions: Option<Extensions>,
        aad: &[u8],
        credential_with_key: CredentialWithKey,
    ) -> Result<(Self, MlsMessageOut, Option<GroupInfo>), ExternalCommitError<Provider::StorageError>>
    {
        // Prepare the commit parameters
        let framing_parameters = FramingParameters::new(aad, WireFormat::PublicMessage);

        let leaf_node_parameters = LeafNodeParameters::builder()
            .with_capabilities(capabilities.unwrap_or_default())
            .with_extensions(extensions.unwrap_or_default())
            .build();
        let params = CreateCommitParams::builder()
            .framing_parameters(framing_parameters)
            .commit_type(CommitType::External(credential_with_key))
            .leaf_node_parameters(leaf_node_parameters)
            .build();
        let (mut group, create_commit_result) = CoreGroup::join_by_external_commit(
            provider,
            signer,
            params,
            ratchet_tree,
            verifiable_group_info,
        )?;
        group.set_max_past_epochs(mls_group_config.max_past_epochs);

        let mls_group = MlsGroup {
            mls_group_config: mls_group_config.clone(),
            group,
            own_leaf_nodes: vec![],
            aad: vec![],
            group_state: MlsGroupState::PendingCommit(Box::new(PendingCommitState::External(
                create_commit_result.staged_commit,
            ))),
        };

        provider
            .storage()
            .write_mls_join_config(mls_group.group_id(), &mls_group.mls_group_config)
            .map_err(ExternalCommitError::StorageError)?;
        provider
            .storage()
            .write_group_state(mls_group.group_id(), &mls_group.group_state)
            .map_err(ExternalCommitError::StorageError)?;
        mls_group
            .group
            .store(provider.storage())
            .map_err(ExternalCommitError::StorageError)?;

        let public_message: PublicMessage = create_commit_result.commit.into();

        Ok((
            mls_group,
            public_message.into(),
            create_commit_result.group_info,
        ))
    }
}

fn transpose_err_opt<T, E>(v: Result<Option<T>, E>) -> Option<Result<T, E>> {
    match v {
        Ok(Some(v)) => Some(Ok(v)),
        Ok(None) => None,
        Err(err) => Some(Err(err)),
    }
}

impl ProcessedWelcome {
    /// Creates a new processed [`Welcome`] message , which can be
    /// inspected before creating a [`StagedWelcome`].
    ///
    /// This does not require a ratchet tree yet.
    ///
    /// [`Welcome`]: crate::messages::Welcome
    pub fn new_from_welcome<Provider: OpenMlsProvider>(
        provider: &Provider,
        mls_group_config: &MlsGroupJoinConfig,
        welcome: Welcome,
    ) -> Result<Self, WelcomeError<Provider::StorageError>> {
        let (resumption_psk_store, key_package_bundle) =
            keys_for_welcome(mls_group_config, &welcome, provider)?;

        let (ciphersuite, group_secrets, key_schedule, verifiable_group_info) =
            crate::group::core_group::new_from_welcome::process_welcome(
                welcome,
                &key_package_bundle,
                provider,
                &resumption_psk_store,
            )?;

        Ok(Self {
            mls_group_config: mls_group_config.clone(),
            ciphersuite,
            group_secrets,
            key_schedule,
            verifiable_group_info,
            resumption_psk_store,
            key_package_bundle,
        })
    }

    /// Get a reference to the GroupInfo in this Welcome message.
    ///
    /// **NOTE:** The group info contains **unverified** values. Use with caution.
    pub fn unverified_group_info(&self) -> &VerifiableGroupInfo {
        &self.verifiable_group_info
    }

    /// Get a reference to the PSKs in this Welcome message.
    ///
    /// **NOTE:** The group info contains **unverified** values. Use with caution.
    pub fn psks(&self) -> &[PreSharedKeyId] {
        &self.group_secrets.psks
    }

    /// Consume the `ProcessedWelcome` and combine it witht he ratchet tree into
    /// a `StagedWelcome`.
    pub fn into_staged_welcome<Provider: OpenMlsProvider>(
        self,
        provider: &Provider,
        ratchet_tree: Option<RatchetTreeIn>,
    ) -> Result<StagedWelcome, WelcomeError<Provider::StorageError>> {
        let group = crate::group::core_group::new_from_welcome::build_staged_welcome(
            self.verifiable_group_info,
            ratchet_tree,
            provider,
            self.key_package_bundle,
            self.key_schedule,
            self.ciphersuite,
            self.resumption_psk_store,
            self.group_secrets,
        )?;

        let staged_welcome = StagedWelcome {
            mls_group_config: self.mls_group_config,
            group,
        };

        Ok(staged_welcome)
    }
}

impl StagedWelcome {
    /// Creates a new staged welcome from a [`Welcome`] message. Returns an error
    /// ([`WelcomeError::NoMatchingKeyPackage`]) if no [`KeyPackage`]
    /// can be found.
    /// Note: calling this function will consume the key material for decrypting the [`Welcome`]
    /// message, even if the caller does not turn the [`StagedWelcome`] into an [`MlsGroup`].
    ///
    /// [`Welcome`]: crate::messages::Welcome
    pub fn new_from_welcome<Provider: OpenMlsProvider>(
        provider: &Provider,
        mls_group_config: &MlsGroupJoinConfig,
        welcome: Welcome,
        ratchet_tree: Option<RatchetTreeIn>,
    ) -> Result<Self, WelcomeError<Provider::StorageError>> {
        let (resumption_psk_store, key_package_bundle) =
            keys_for_welcome(mls_group_config, &welcome, provider)?;

        let group = StagedCoreWelcome::new_from_welcome(
            welcome,
            ratchet_tree,
            key_package_bundle,
            provider,
            resumption_psk_store,
        )?;

        let staged_welcome = StagedWelcome {
            mls_group_config: mls_group_config.clone(),
            group,
        };

        Ok(staged_welcome)
    }

    /// Returns the [`LeafNodeIndex`] of the group member that authored the [`Welcome`] message.
    ///
    /// [`Welcome`]: crate::messages::Welcome
    pub fn welcome_sender_index(&self) -> LeafNodeIndex {
        self.group.welcome_sender_index()
    }

    /// Returns the [`LeafNode`] of the group member that authored the [`Welcome`] message.
    ///
    /// [`Welcome`]: crate::messages::Welcome
    pub fn welcome_sender(&self) -> Result<&LeafNode, LibraryError> {
        self.group.welcome_sender()
    }

    /// Consumes the [`StagedWelcome`] and returns the respective [`MlsGroup`].
    pub fn into_group<Provider: OpenMlsProvider>(
        self,
        provider: &Provider,
    ) -> Result<MlsGroup, WelcomeError<Provider::StorageError>> {
        let mut group = self.group.into_core_group(provider)?;
        group.set_max_past_epochs(self.mls_group_config.max_past_epochs);

        let mls_group = MlsGroup {
            mls_group_config: self.mls_group_config,
            group,
            own_leaf_nodes: vec![],
            aad: vec![],
            group_state: MlsGroupState::Operational,
        };

        provider
            .storage()
            .write_mls_join_config(mls_group.group_id(), &mls_group.mls_group_config)
            .map_err(WelcomeError::StorageError)?;
        provider
            .storage()
            .write_group_state(mls_group.group_id(), &MlsGroupState::Operational)
            .map_err(WelcomeError::StorageError)?;

        Ok(mls_group)
    }
}

fn keys_for_welcome<Provider: OpenMlsProvider>(
    mls_group_config: &MlsGroupJoinConfig,
    welcome: &Welcome,
    provider: &Provider,
) -> Result<
    (ResumptionPskStore, KeyPackageBundle),
    WelcomeError<<Provider as OpenMlsProvider>::StorageError>,
> {
    let resumption_psk_store = ResumptionPskStore::new(mls_group_config.number_of_resumption_psks);
    let key_package_bundle: KeyPackageBundle = welcome
        .secrets()
        .iter()
        .find_map(|egs| {
            let hash_ref = egs.new_member();

            transpose_err_opt(
                provider
                    .storage()
                    .key_package(&hash_ref)
                    .map_err(WelcomeError::StorageError),
            )
        })
        .ok_or(WelcomeError::NoMatchingKeyPackage)??;
    if !key_package_bundle.key_package().last_resort() {
        provider
            .storage()
            .delete_key_package(&key_package_bundle.key_package.hash_ref(provider.crypto())?)
            .map_err(WelcomeError::StorageError)?;
    } else {
        log::debug!("Key package has last resort extension, not deleting");
    }
    Ok((resumption_psk_store, key_package_bundle))
}
