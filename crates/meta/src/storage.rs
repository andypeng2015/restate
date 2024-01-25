// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use codederror::CodedError;
use restate_schema_impl::SchemasUpdateCommand;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use tokio::io;
use tracing::log::info;
use tracing::trace;

type StorageFormatVersion = u32;

/// Storage format version used by the [`FileMetaStorage`] to store schema information. This value
/// must be incremented whenever you introduce a breaking change to the schema information.
const STORAGE_FORMAT_VERSION: StorageFormatVersion = 1;

/// Name of the file which contains the storage format version.
const STORAGE_FORMAT_VERSION_FILE_NAME: &str = ".meta_format_version";

#[derive(Debug, thiserror::Error)]
pub enum MetaStorageError {
    #[error("generic io error: {0}")]
    Io(#[from] io::Error),
    #[error("generic serde error: {0}. This is probably a runtime bug")]
    Encode(#[from] bincode::error::EncodeError),
    #[error("generic serde error: {0}. This is probably a runtime bug")]
    Decode(#[from] bincode::error::DecodeError),
    #[error("generic descriptor error: {0}. This is probably a runtime bug")]
    Descriptor(#[from] prost_reflect::DescriptorError),
    #[error("task error when writing to disk: {0}. This is probably a runtime bug")]
    Join(#[from] tokio::task::JoinError),
    #[error("file ending with .restate has a bad filename: {0}. This is probably a runtime bug")]
    BadFilename(PathBuf),
}

pub trait MetaStorage {
    fn store(
        &mut self,
        commands: Vec<SchemasUpdateCommand>,
    ) -> impl Future<Output = Result<(), MetaStorageError>> + Send;

    fn reload(
        &mut self,
    ) -> impl Future<Output = Result<Vec<SchemasUpdateCommand>, MetaStorageError>> + Send;
}

// --- File based implementation of MetaStorage, using bincode

#[derive(Debug, thiserror::Error, CodedError)]
pub enum BuildError {
    #[error("storage directory contains incompatible storage format version '{0}'; supported version is '{STORAGE_FORMAT_VERSION}'")]
    #[code(restate_errors::META0010)]
    IncompatibleStorageFormat(StorageFormatVersion),
    #[error("generic io error: {0}")]
    #[code(unknown)]
    Io(#[from] io::Error),
    #[error("serde error: {0}")]
    #[code(unknown)]
    Serde(#[from] serde_json::Error),
}

const RESTATE_EXTENSION: &str = "restate";

#[derive(Debug)]
pub struct FileMetaStorage {
    root_path: PathBuf,
    next_file_index: usize,
}

impl FileMetaStorage {
    pub fn new(root_path: PathBuf) -> Result<Self, BuildError> {
        if Self::is_empty_directory(root_path.as_path()) {
            Self::write_storage_format_version_to_file(
                root_path.as_path(),
                STORAGE_FORMAT_VERSION,
            )?;
        } else {
            Self::assert_compatible_storage_format_version(root_path.as_path())?;
        }

        Ok(Self {
            root_path,
            next_file_index: 0,
        })
    }

    fn is_empty_directory(path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();

        !path.exists()
            || path
                .read_dir()
                .expect("meta storage directory must exist")
                .count()
                == 0
    }

    fn write_storage_format_version_to_file(
        root_path: impl AsRef<Path>,
        version: StorageFormatVersion,
    ) -> Result<(), io::Error> {
        let root_path = root_path.as_ref();

        // make sure that the root directory exists
        std::fs::create_dir_all(root_path)?;

        let version_file_path = root_path.join(STORAGE_FORMAT_VERSION_FILE_NAME);
        assert!(
            !version_file_path.exists(),
            "must never overwrite an existing version file"
        );

        let version_file = std::fs::File::create(version_file_path)?;

        // use a human readable format
        serde_json::to_writer(version_file, &version)?;

        Ok(())
    }

    fn assert_compatible_storage_format_version(
        root_path: impl AsRef<Path>,
    ) -> Result<(), BuildError> {
        let version_file =
            std::fs::File::open(root_path.as_ref().join(STORAGE_FORMAT_VERSION_FILE_NAME));

        let version = if let Ok(version_file) = version_file {
            serde_json::from_reader(version_file)?
        } else {
            // File does not exist, this indicates that the data has been written with a Restate
            // version <= 0.7 that does not write a version file. Write it now for future
            // compatibility.
            info!("Opened file meta storage w/o a version file present. This indicates that the data has been written with a Restate version <= 0.7.0. Assuming the format version to be 1.");
            Self::write_storage_format_version_to_file(root_path, 1)?;
            1
        };

        if version != STORAGE_FORMAT_VERSION {
            Err(BuildError::IncompatibleStorageFormat(version))
        } else {
            Ok(())
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct CommandsFile(Vec<SchemasUpdateCommand>);

impl MetaStorage for FileMetaStorage {
    async fn store(&mut self, commands: Vec<SchemasUpdateCommand>) -> Result<(), MetaStorageError> {
        let file_path = self
            .root_path
            .join(format!("{}.{}", self.next_file_index, RESTATE_EXTENSION));
        self.next_file_index += 1;

        trace!("Write metadata file {}", file_path.display());

        // We use blocking spawn to use bincode::encode_into_std_write
        tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::create(file_path)?;
            bincode::serde::encode_into_std_write(
                CommandsFile(commands),
                &mut file,
                bincode::config::standard(),
            )?;
            Result::<(), MetaStorageError>::Ok(file.sync_all()?)
        })
        .await??;
        Ok(())
    }

    async fn reload(&mut self) -> Result<Vec<SchemasUpdateCommand>, MetaStorageError> {
        let root_path = self.root_path.clone();

        // Try to create a dir, in case it doesn't exist
        restate_fs_util::create_dir_all_if_doesnt_exists(&root_path).await?;

        // Find all the metadata files in the root path directory, parse the index and then sort them by index
        let mut read_dir = tokio::fs::read_dir(root_path).await?;
        let mut metadata_files = vec![];
        while let Some(dir_entry) = read_dir.next_entry().await? {
            if dir_entry
                .path()
                .extension()
                .and_then(|os_str| os_str.to_str())
                == Some(RESTATE_EXTENSION)
            {
                let index: usize = dir_entry
                    .path()
                    .file_stem()
                    .expect("If there is an extension, there must be a file stem")
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| MetaStorageError::BadFilename(dir_entry.path()))?;

                // Make sure self.next_file_index = max(self.next_file_index, index + 1)
                self.next_file_index = self.next_file_index.max(index + 1);
                metadata_files.push((dir_entry.path(), index));
            }
        }
        metadata_files.sort_by(|a, b| a.1.cmp(&b.1));

        // We use blocking spawn to use bincode::decode_from_std_read
        tokio::task::spawn_blocking(move || {
            let mut schemas_updates = vec![];

            for (metadata_file_path, _) in metadata_files {
                // Metadata_file_path is the json metadata descriptor
                trace!("Reloading metadata file {}", metadata_file_path.display());

                let mut file = std::fs::File::open(metadata_file_path)?;

                let commands_file: CommandsFile =
                    bincode::serde::decode_from_std_read(&mut file, bincode::config::standard())?;
                schemas_updates.extend(commands_file.0);
            }

            Result::<Vec<SchemasUpdateCommand>, MetaStorageError>::Ok(schemas_updates)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use googletest::matchers::eq;
    use googletest::{assert_that, pat};
    use tempfile::tempdir;
    use test_log::test;

    use restate_pb::mocks;
    use restate_schema_api::deployment::Deployment;
    use restate_schema_impl::Schemas;

    #[test(tokio::test)]
    async fn reload_in_order() {
        let schemas = Schemas::default();
        let temp_dir = tempdir().unwrap();
        let mut file_storage =
            FileMetaStorage::new(temp_dir.path().to_path_buf()).expect("file storage should build");

        // Generate some commands for a new deployment, with new services
        let deployment_1 = Deployment::mock_with_uri("http://localhost:9080");
        let commands_1 = schemas
            .compute_new_deployment(
                Some(deployment_1.id),
                deployment_1.metadata,
                vec![mocks::GREETER_SERVICE_NAME.to_owned()],
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();

        file_storage.store(commands_1.clone()).await.unwrap();

        // Generate some commands for a new deployment, with a new and old service
        // We need to apply updates to generate a new command list
        schemas.apply_updates(commands_1.clone()).unwrap();
        let deployment_2 = Deployment::mock_with_uri("http://localhost:9081");
        let commands_2 = schemas
            .compute_new_deployment(
                Some(deployment_2.id),
                deployment_2.metadata,
                vec![
                    mocks::GREETER_SERVICE_NAME.to_owned(),
                    mocks::ANOTHER_GREETER_SERVICE_NAME.to_owned(),
                ],
                mocks::DESCRIPTOR_POOL.clone(),
                false,
            )
            .unwrap();

        file_storage.store(commands_2.clone()).await.unwrap();

        // Check we can apply these commands
        schemas.apply_updates(commands_2.clone()).unwrap();

        let mut expected_commands = vec![];
        expected_commands.extend(commands_1);
        expected_commands.extend(commands_2);
        let expected_commands: Vec<SchemasUpdateCommandEquality> =
            expected_commands.into_iter().map(Into::into).collect();

        // Now let's try to reload
        let mut file_storage =
            FileMetaStorage::new(temp_dir.path().to_path_buf()).expect("file storage should build");
        let actual_commands = file_storage.reload().await.unwrap();

        assert_eq!(
            actual_commands
                .into_iter()
                .map(SchemasUpdateCommandEquality::from)
                .collect::<Vec<_>>(),
            expected_commands
        );
    }

    // Newtype to implement equality for the scope of this test
    #[derive(Debug)]
    struct SchemasUpdateCommandEquality(SchemasUpdateCommand);

    impl From<SchemasUpdateCommand> for SchemasUpdateCommandEquality {
        fn from(value: SchemasUpdateCommand) -> Self {
            Self(value)
        }
    }

    impl PartialEq for SchemasUpdateCommandEquality {
        fn eq(&self, other: &Self) -> bool {
            match (&self.0, &other.0) {
                (
                    SchemasUpdateCommand::InsertDeployment {
                        deployment_id: self_deployment_id,
                        services: self_services,
                        ..
                    },
                    SchemasUpdateCommand::InsertDeployment {
                        deployment_id: other_deployment_id,
                        services: other_services,
                        ..
                    },
                ) => self_deployment_id == other_deployment_id && self_services == other_services,
                (
                    SchemasUpdateCommand::RemoveService {
                        name: self_name,
                        revision: self_revision,
                    },
                    SchemasUpdateCommand::RemoveService {
                        name: other_name,
                        revision: other_revision,
                    },
                ) => self_name == other_name && self_revision == other_revision,
                _ => false,
            }
        }
    }

    impl Eq for SchemasUpdateCommandEquality {}

    #[test]
    fn incompatible_storage_format_version() -> anyhow::Result<()> {
        let tempdir = tempdir()?;

        let incompatible_storage_format_version = STORAGE_FORMAT_VERSION + 1;
        FileMetaStorage::write_storage_format_version_to_file(
            tempdir.path(),
            incompatible_storage_format_version,
        )?;

        let build_error = FileMetaStorage::new(tempdir.into_path())
            .expect_err("should have failed with incompatible storage format version");

        assert_that!(
            build_error,
            pat!(BuildError::IncompatibleStorageFormat(eq(
                incompatible_storage_format_version
            )))
        );

        Ok(())
    }
}
