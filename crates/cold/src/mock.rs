// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use gpucache_core_types::{content_checksum, CacheError};

use crate::pack::{ObjectMeta, ObjectStore};

/// In-memory stand-in for RustFS. A passing mock is not a hardware run.
#[derive(Clone, Debug, Default)]
pub struct MockObjectStore {
    objects: BTreeMap<String, Vec<u8>>,
}

impl ObjectStore for MockObjectStore {
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
        self.objects.insert(key.to_string(), body.to_vec());
        Ok(())
    }

    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError> {
        let body = self.objects.get(key).ok_or(CacheError::Storage {
            reason: "object missing",
        })?;
        Ok(ObjectMeta {
            len: body.len() as u64,
            checksum: content_checksum(body),
        })
    }

    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        let body = self.objects.get(key).ok_or(CacheError::Storage {
            reason: "object missing",
        })?;
        let start = usize::try_from(offset).map_err(|_| CacheError::Malformed {
            reason: "cold range",
        })?;
        let len = usize::try_from(length).map_err(|_| CacheError::Malformed {
            reason: "cold range",
        })?;
        let end = start.checked_add(len).ok_or(CacheError::Malformed {
            reason: "cold range",
        })?;
        if end > body.len() {
            return Err(CacheError::LengthMismatch {
                expected: body.len() as u64,
                actual: end as u64,
            });
        }
        Ok(body[start..end].to_vec())
    }

    fn delete(&mut self, key: &str) -> Result<(), CacheError> {
        self.objects.remove(key);
        Ok(())
    }
}
