/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;

use edenapi_types::ContentId;
use edenapi_types::Sha1;
use minibytes::Bytes;
use serde_derive::Deserialize;
use serde_derive::Serialize;
use types::Key;
use types::Sha256;

/// Kind of content hash stored in the LFS pointer. Adding new types is acceptable, re-ordering or
/// removal is forbidden.
#[derive(
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Debug,
    Hash,
    Ord,
    PartialOrd,
    Clone
)]
pub enum ContentHash {
    Sha256(#[serde(with = "types::serde_with::sha256::tuple")] Sha256),
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, Ord, PartialOrd)]
pub enum StoreKey {
    HgId(Key),
    /// The Key is a temporary workaround to being able to fallback from the LFS protocol to the
    /// non-LFS protocol. Do not depend on it as it will be removed.
    Content(ContentHash, Option<Key>),
}

impl ContentHash {
    pub fn sha256(data: &Bytes) -> Self {
        use sha2::Digest;

        let mut hash = sha2::Sha256::new();
        hash.input(data);

        let bytes: [u8; Sha256::len()] = hash.result().into();
        ContentHash::Sha256(Sha256::from(bytes))
    }

    pub(crate) fn content_id(data: &Bytes) -> ContentId {
        use blake2::digest::Input;
        use blake2::digest::VariableOutput;
        use blake2::VarBlake2b;

        // cribbed from pyedenapi
        let mut hash = VarBlake2b::new_keyed(b"content", ContentId::len());
        hash.input(data);
        let mut ret = [0u8; ContentId::len()];
        hash.variable_result(|res| {
            if let Err(e) = ret.as_mut().write_all(res) {
                panic!(
                    "{}-byte array must work with {}-byte blake2b: {:?}",
                    ContentId::len(),
                    ContentId::len(),
                    e
                );
            }
        });
        ContentId::from(ret)
    }

    pub(crate) fn sha1(data: &Bytes) -> Sha1 {
        use sha1::Digest;

        let mut hash = sha1::Sha1::new();
        hash.input(data);

        let bytes: [u8; Sha1::len()] = hash.result().into();
        Sha1::from(bytes)
    }

    pub fn unwrap_sha256(self) -> Sha256 {
        match self {
            ContentHash::Sha256(hash) => hash,
        }
    }
}

impl StoreKey {
    pub fn hgid(key: Key) -> Self {
        StoreKey::HgId(key)
    }

    pub fn content(hash: ContentHash) -> Self {
        StoreKey::Content(hash, None)
    }

    pub fn maybe_into_key(self) -> Option<Key> {
        match self {
            StoreKey::HgId(key) => Some(key),
            StoreKey::Content(_, k) => k,
        }
    }

    pub fn maybe_as_key(&self) -> Option<&Key> {
        match self {
            StoreKey::HgId(ref key) => Some(key),
            StoreKey::Content(_, ref k) => k.as_ref(),
        }
    }
}

impl From<Key> for StoreKey {
    fn from(k: Key) -> Self {
        StoreKey::HgId(k)
    }
}

impl<'a> From<&'a Key> for StoreKey {
    fn from(k: &'a Key) -> Self {
        StoreKey::HgId(k.clone())
    }
}

impl From<ContentHash> for StoreKey {
    fn from(hash: ContentHash) -> Self {
        StoreKey::Content(hash, None)
    }
}

impl<'a> From<&'a ContentHash> for StoreKey {
    fn from(hash: &'a ContentHash) -> Self {
        StoreKey::Content(hash.clone(), None)
    }
}

#[cfg(any(test, feature = "for-tests"))]
impl quickcheck::Arbitrary for ContentHash {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        ContentHash::Sha256(Sha256::arbitrary(g))
    }
}

#[cfg(any(test, feature = "for-tests"))]
impl quickcheck::Arbitrary for StoreKey {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        if bool::arbitrary(g) {
            StoreKey::HgId(Key::arbitrary(g))
        } else {
            StoreKey::Content(ContentHash::arbitrary(g), Option::arbitrary(g))
        }
    }
}
