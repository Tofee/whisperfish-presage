use std::{
    ops::{Bound, RangeBounds, RangeFull},
    sync::Arc,
};

use presage::{
    libsignal_service::{
        content::Content,
        prelude::Uuid,
        zkgroup::{profiles::ProfileKey, GroupMasterKeyBytes},
        Profile,
    },
    model::{contacts::Contact, groups::Group},
    store::{ContentExt, ContentsStore, StickerPack, Thread},
    AvatarBytes,
    
};
use presage::ThreadMetadata;

use prost::Message;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use sled::IVec;
use tracing::{debug, trace};

use crate::{protobuf::ContentProto, SledStore, SledStoreError};

const SLED_TREE_PROFILE_AVATARS: &str = "profile_avatars";
const SLED_TREE_PROFILE_KEYS: &str = "profile_keys";
const SLED_TREE_STICKER_PACKS: &str = "sticker_packs";
const SLED_TREE_CONTACTS: &str = "contacts";
const SLED_TREE_GROUP_AVATARS: &str = "group_avatars";
const SLED_TREE_GROUPS: &str = "groups";
const SLED_TREE_PROFILES: &str = "profiles";
const SLED_TREE_THREADS_PREFIX: &str = "threads";
const SLED_TREE_THREADS_METADATA: &str = "threads_metadata";


impl ContentsStore for SledStore {
    type ContentsStoreError = SledStoreError;

    type ContactsIter = SledContactsIter;
    type GroupsIter = SledGroupsIter;
    type MessagesIter = SledMessagesIter;
    type StickerPacksIter = SledStickerPacksIter;
    type ThreadMetadataIter = SledThreadMetadataIter;

    async fn clear_profiles(&mut self) -> Result<(), Self::ContentsStoreError> {
        let db = self.write();
        db.drop_tree(SLED_TREE_PROFILES)?;
        db.drop_tree(SLED_TREE_PROFILE_KEYS)?;
        db.drop_tree(SLED_TREE_PROFILE_AVATARS)?;
        db.flush()?;
        Ok(())
    }

    async fn clear_contents(&mut self) -> Result<(), Self::ContentsStoreError> {
        let db = self.write();
        db.drop_tree(SLED_TREE_CONTACTS)?;
        db.drop_tree(SLED_TREE_GROUPS)?;

        for tree in db
            .tree_names()
            .into_iter()
            .filter(|n| n.starts_with(SLED_TREE_THREADS_PREFIX.as_bytes()))
        {
            db.drop_tree(tree)?;
        }

        db.flush()?;
        Ok(())
    }

    async fn clear_contacts(&mut self) -> Result<(), SledStoreError> {
        self.write().drop_tree(SLED_TREE_CONTACTS)?;
        Ok(())
    }

    async fn save_contact(&mut self, contact: &Contact) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_CONTACTS, contact.uuid, contact)?;
        debug!("saved contact");
        Ok(())
    }

    async fn contacts(&self) -> Result<Self::ContactsIter, SledStoreError> {
        Ok(SledContactsIter {
            iter: self.read().open_tree(SLED_TREE_CONTACTS)?.iter(),
            #[cfg(feature = "encryption")]
            cipher: self.cipher.clone(),
        })
    }

    async fn contact_by_id(&self, id: &Uuid) -> Result<Option<Contact>, SledStoreError> {
        self.get(SLED_TREE_CONTACTS, id)
    }

    // Groups

    async fn clear_groups(&mut self) -> Result<(), SledStoreError> {
        let db = self.write();
        db.drop_tree(SLED_TREE_GROUPS)?;
        db.flush()?;
        Ok(())
    }

    async fn groups(&self) -> Result<Self::GroupsIter, SledStoreError> {
        Ok(SledGroupsIter {
            iter: self.read().open_tree(SLED_TREE_GROUPS)?.iter(),
            #[cfg(feature = "encryption")]
            cipher: self.cipher.clone(),
        })
    }

    async fn group(
        &self,
        master_key_bytes: GroupMasterKeyBytes,
    ) -> Result<Option<Group>, SledStoreError> {
        self.get(SLED_TREE_GROUPS, master_key_bytes)
    }

    async fn save_group(
        &self,
        master_key: GroupMasterKeyBytes,
        group: impl Into<Group>,
    ) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_GROUPS, master_key, group.into())?;
        Ok(())
    }

    async fn group_avatar(
        &self,
        master_key_bytes: GroupMasterKeyBytes,
    ) -> Result<Option<AvatarBytes>, SledStoreError> {
        self.get(SLED_TREE_GROUP_AVATARS, master_key_bytes)
    }

    async fn save_group_avatar(
        &self,
        master_key: GroupMasterKeyBytes,
        avatar: &AvatarBytes,
    ) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_GROUP_AVATARS, master_key, avatar)?;
        Ok(())
    }

    // Messages

    async fn clear_messages(&mut self) -> Result<(), SledStoreError> {
        let db = self.write();
        for name in db.tree_names() {
            if name
                .as_ref()
                .starts_with(SLED_TREE_THREADS_PREFIX.as_bytes())
            {
                db.drop_tree(&name)?;
            }
        }
        db.flush()?;
        Ok(())
    }

    async fn clear_thread(&mut self, thread: &Thread) -> Result<(), SledStoreError> {
        trace!(%thread, "clearing thread");

        let db = self.write();
        db.drop_tree(messages_thread_tree_name(thread))?;
        db.flush()?;

        Ok(())
    }

    async fn save_message(&self, thread: &Thread, message: Content) -> Result<(), SledStoreError> {
        let ts = message.timestamp();
        trace!(%thread, ts, "storing a message with thread");

        let tree = messages_thread_tree_name(thread);
        let key = ts.to_be_bytes();

        let proto: ContentProto = message.into();
        let value = proto.encode_to_vec();

        self.insert(&tree, key, value)?;

        Ok(())
    }

    async fn delete_message(
        &mut self,
        thread: &Thread,
        timestamp: u64,
    ) -> Result<bool, SledStoreError> {
        let tree = messages_thread_tree_name(thread);
        self.remove(&tree, timestamp.to_be_bytes())
    }

    async fn message(
        &self,
        thread: &Thread,
        timestamp: u64,
    ) -> Result<Option<Content>, SledStoreError> {
        // Big-Endian needed, otherwise wrong ordering in sled.
        let val: Option<Vec<u8>> =
            self.get(&messages_thread_tree_name(thread), timestamp.to_be_bytes())?;
        match val {
            Some(ref v) => {
                let proto = ContentProto::decode(v.as_slice())?;
                let content = proto.try_into()?;
                Ok(Some(content))
            }
            None => Ok(None),
        }
    }

    async fn messages(
        &self,
        thread: &Thread,
        range: impl RangeBounds<u64>,
    ) -> Result<Self::MessagesIter, SledStoreError> {
        let tree_thread = self.read().open_tree(messages_thread_tree_name(thread))?;
        debug!(%thread, count = tree_thread.len(), "loading message tree");

        let iter = match (range.start_bound(), range.end_bound()) {
            (Bound::Included(start), Bound::Unbounded) => tree_thread.range(start.to_be_bytes()..),
            (Bound::Included(start), Bound::Excluded(end)) => {
                tree_thread.range(start.to_be_bytes()..end.to_be_bytes())
            }
            (Bound::Included(start), Bound::Included(end)) => {
                tree_thread.range(start.to_be_bytes()..=end.to_be_bytes())
            }
            (Bound::Unbounded, Bound::Included(end)) => tree_thread.range(..=end.to_be_bytes()),
            (Bound::Unbounded, Bound::Excluded(end)) => tree_thread.range(..end.to_be_bytes()),
            (Bound::Unbounded, Bound::Unbounded) => tree_thread.range::<[u8; 8], RangeFull>(..),
            (Bound::Excluded(_), _) => {
                unreachable!("range that excludes the initial value")
            }
        };

        Ok(SledMessagesIter {
            #[cfg(feature = "encryption")]
            cipher: self.cipher.clone(),
            iter,
        })
    }

    async fn upsert_profile_key(
        &mut self,
        uuid: &Uuid,
        key: ProfileKey,
    ) -> Result<bool, SledStoreError> {
        self.insert(SLED_TREE_PROFILE_KEYS, uuid.as_bytes(), key)
    }

    async fn profile_key(&self, uuid: &Uuid) -> Result<Option<ProfileKey>, SledStoreError> {
        self.get(SLED_TREE_PROFILE_KEYS, uuid.as_bytes())
    }

    async fn save_profile(
        &mut self,
        uuid: Uuid,
        key: ProfileKey,
        profile: Profile,
    ) -> Result<(), SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.insert(SLED_TREE_PROFILES, key, profile)?;
        Ok(())
    }

    async fn profile(
        &self,
        uuid: Uuid,
        key: ProfileKey,
    ) -> Result<Option<Profile>, SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.get(SLED_TREE_PROFILES, key)
    }

    async fn save_profile_avatar(
        &mut self,
        uuid: Uuid,
        key: ProfileKey,
        avatar: &AvatarBytes,
    ) -> Result<(), SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.insert(SLED_TREE_PROFILE_AVATARS, key, avatar)?;
        Ok(())
    }

    async fn profile_avatar(
        &self,
        uuid: Uuid,
        key: ProfileKey,
    ) -> Result<Option<AvatarBytes>, SledStoreError> {
        let key = self.profile_key_for_uuid(uuid, key);
        self.get(SLED_TREE_PROFILE_AVATARS, key)
    }

    async fn add_sticker_pack(&mut self, pack: &StickerPack) -> Result<(), SledStoreError> {
        self.insert(SLED_TREE_STICKER_PACKS, pack.id.clone(), pack)?;
        Ok(())
    }

    async fn remove_sticker_pack(&mut self, id: &[u8]) -> Result<bool, SledStoreError> {
        self.remove(SLED_TREE_STICKER_PACKS, id)
    }

    async fn sticker_pack(&self, id: &[u8]) -> Result<Option<StickerPack>, SledStoreError> {
        self.get(SLED_TREE_STICKER_PACKS, id)
    }

    async fn sticker_packs(&self) -> Result<Self::StickerPacksIter, SledStoreError> {
        Ok(SledStickerPacksIter {
            cipher: self.cipher.clone(),
            iter: self.read().open_tree(SLED_TREE_STICKER_PACKS)?.iter(),
        })
    }

    fn save_thread_metadata(
        &mut self,
        metadata: ThreadMetadata,
    ) -> Result<(), Self::ContentsStoreError> {
        let key = self.thread_metadata_key(metadata.thread.clone());
        self.insert(SLED_TREE_THREADS_METADATA, key, metadata)?;
        Ok(())
    }

    fn thread_metadata(
        &self,
        thread: Thread,
    ) -> Result<Option<ThreadMetadata>, Self::ContentsStoreError> {
        let key = self.thread_metadata_key(thread);
        self.get(SLED_TREE_THREADS_METADATA, key)
    }

    fn thread_metadatas(&self) -> Result<Self::ThreadMetadataIter, SledStoreError> {
        let tree = self.read().open_tree(SLED_TREE_THREADS_METADATA)?;
        let iter = tree.iter();
        Ok(SledThreadMetadataIter {
            cipher: self.cipher.clone(),
            iter,
        })
    }
}

pub struct SledContactsIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl SledContactsIter {
    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }
}

impl Iterator for SledContactsIter {
    type Item = Result<Contact, SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| self.decrypt_value(&value))
            .into()
    }
}

pub struct SledGroupsIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl SledGroupsIter {
    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }
}

impl Iterator for SledGroupsIter {
    type Item = Result<(GroupMasterKeyBytes, Group), SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.iter.next()?.map_err(SledStoreError::from).and_then(
            |(group_master_key_bytes, value)| {
                let group = self.decrypt_value(&value)?;
                Ok((
                    group_master_key_bytes
                        .as_ref()
                        .try_into()
                        .map_err(|_| SledStoreError::GroupDecryption)?,
                    group,
                ))
            },
        ))
    }
}

pub struct SledStickerPacksIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl Iterator for SledStickerPacksIter {
    type Item = Result<StickerPack, SledStoreError>;

    #[cfg(feature = "encryption")]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| {
                if let Some(cipher) = self.cipher.as_ref() {
                    cipher.decrypt_value(&value).map_err(SledStoreError::from)
                } else {
                    serde_json::from_slice(&value).map_err(SledStoreError::from)
                }
            })
            .into()
    }

    #[cfg(not(feature = "encryption"))]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| serde_json::from_slice(&value).map_err(SledStoreError::from))
            .into()
    }
}

pub struct SledMessagesIter {
    #[cfg(feature = "encryption")]
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl SledMessagesIter {
    #[cfg(feature = "encryption")]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        if let Some(cipher) = self.cipher.as_ref() {
            Ok(cipher.decrypt_value(value)?)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, SledStoreError> {
        Ok(serde_json::from_slice(value)?)
    }
}

impl SledMessagesIter {
    fn decode(
        &self,
        elem: Result<(IVec, IVec), sled::Error>,
    ) -> Option<Result<Content, SledStoreError>> {
        elem.map_err(SledStoreError::from)
            .and_then(|(_, value)| self.decrypt_value(&value).map_err(SledStoreError::from))
            .and_then(|data: Vec<u8>| ContentProto::decode(&data[..]).map_err(SledStoreError::from))
            .map_or_else(|e| Some(Err(e)), |p| Some(p.try_into()))
    }
}

impl Iterator for SledMessagesIter {
    type Item = Result<Content, SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        let elem = self.iter.next()?;
        self.decode(elem)
    }
}

impl DoubleEndedIterator for SledMessagesIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        let elem = self.iter.next_back()?;
        self.decode(elem)
    }
}

fn messages_thread_tree_name(t: &Thread) -> String {
    use base64::prelude::*;
    let key = match t {
        Thread::Contact(uuid) => {
            format!("{SLED_TREE_THREADS_PREFIX}:contact:{uuid}")
        }
        Thread::Group(group_id) => format!(
            "{SLED_TREE_THREADS_PREFIX}:group:{}",
            BASE64_STANDARD.encode(group_id)
        ),
    };
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{SLED_TREE_THREADS_PREFIX}:{:x}", hasher.finalize())
}

pub struct SledThreadMetadataIter {
    cipher: Option<Arc<presage_store_cipher::StoreCipher>>,
    iter: sled::Iter,
}

impl Iterator for SledThreadMetadataIter {
    type Item = Result<ThreadMetadata, SledStoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()?
            .map_err(SledStoreError::from)
            .and_then(|(_key, value)| {
                self.cipher.as_ref().map_or_else(
                    || serde_json::from_slice(&value).map_err(SledStoreError::from),
                    |c| c.decrypt_value(&value).map_err(SledStoreError::from),
                )
            })
            .into()
    }
}
