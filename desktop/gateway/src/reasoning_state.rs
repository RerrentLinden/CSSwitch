use std::{
    ffi::{CStr, CString},
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use getrandom::getrandom;
use hmac::{Hmac, Mac};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

const MAGIC: &[u8] = b"CSSWITCH-ANTHROPIC-REASONING-V1\n";
const NONCE_BYTES: usize = 12;
const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_REASONING_BLOCKS: usize = 128;
const MAX_CONTENT_BLOCKS: usize = 1024;
const MAX_MODEL_BYTES: usize = 512;
const MAX_ID_BYTES: usize = 512;
const MAX_TOOL_NAME_BYTES: usize = 512;
const MAX_FINGERPRINT_INPUT_BYTES: usize = 64 * 1024 * 1024;
const ENTRY_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) enum RestorePolicy {
    DeepSeekToolUse,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredBlock {
    index: usize,
    block: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredEntry {
    version: u8,
    created_at: u64,
    fingerprint: String,
    model: String,
    policy: RestorePolicy,
    tools: Vec<ToolBinding>,
    #[serde(default)]
    no_reasoning: bool,
    blocks: Vec<StoredBlock>,
}

#[derive(Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolBinding {
    id: String,
    name: String,
    input_digest: String,
}

pub(crate) struct ReasoningStore {
    root: PathBuf,
    directory: Arc<File>,
    encryption_key: [u8; 32],
    lookup_key: [u8; 32],
    io_lock: Arc<Mutex<()>>,
}

impl fmt::Debug for ReasoningStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReasoningStore([REDACTED])")
    }
}

impl Drop for ReasoningStore {
    fn drop(&mut self) {
        self.encryption_key.zeroize();
        self.lookup_key.zeroize();
    }
}

pub(crate) struct PendingCommit {
    target: String,
    encrypted: Vec<u8>,
}

pub(crate) struct CommittedEntry {
    root: PathBuf,
    directory: Arc<File>,
    target: String,
    previous: Option<Vec<u8>>,
    evicted: Vec<(String, Vec<u8>)>,
    written_digest: [u8; 32],
    io_lock: Arc<Mutex<()>>,
}

impl CommittedEntry {
    pub(crate) fn rollback(self) -> Result<(), String> {
        let CommittedEntry {
            root,
            directory,
            target,
            previous,
            evicted,
            written_digest,
            io_lock,
        } = self;
        let _guard = io_lock
            .lock()
            .map_err(|_| "thinking continuity store lock is unavailable".to_string())?;
        validate_private_root_handle(&directory)?;
        let current = match read_private_file(&directory, &root, &target) {
            Ok(Some(current)) if digest(&current) == written_digest => current,
            Ok(Some(_)) => {
                restore_evicted_entries(
                    &directory,
                    &root,
                    evicted,
                    "thinking continuity eviction rollback failed",
                )?;
                return Err("thinking continuity committed entry changed before rollback".into());
            }
            Ok(None) => {
                restore_evicted_entries(
                    &directory,
                    &root,
                    evicted,
                    "thinking continuity eviction rollback failed",
                )?;
                return Err("thinking continuity committed entry is missing".into());
            }
            Err(error) => {
                restore_evicted_entries(
                    &directory,
                    &root,
                    evicted,
                    "thinking continuity eviction rollback failed",
                )?;
                return Err(error);
            }
        };
        debug_assert_eq!(digest(&current), written_digest);
        let target_result = if let Some(previous) = previous {
            atomic_replace(&directory, &root, &target, &previous)
        } else {
            unlink_private_file(&directory, &root, &target)
                .map_err(|_| "thinking continuity rollback failed".to_string())
                .and_then(|_| sync_directory(&directory))
        };
        let eviction_result = restore_evicted_entries(
            &directory,
            &root,
            evicted,
            "thinking continuity eviction rollback failed",
        );
        target_result.and(eviction_result)
    }
}

impl ReasoningStore {
    pub(crate) fn open(
        root: &Path,
        credential: &str,
        contract: &str,
        endpoint: &str,
        profile_scope: &str,
    ) -> Result<Self, String> {
        if credential.is_empty()
            || contract.is_empty()
            || endpoint.is_empty()
            || profile_scope.is_empty()
        {
            return Err("thinking continuity key scope is unavailable".into());
        }
        let directory = Arc::new(open_private_root(root)?);
        let store = Self {
            root: root.to_path_buf(),
            directory,
            encryption_key: derive_scope_key(
                credential,
                b"csswitch/anthropic-reasoning-sidecar/aead/v1\0",
                contract,
                endpoint,
                profile_scope,
            )?,
            lookup_key: derive_scope_key(
                credential,
                b"csswitch/anthropic-reasoning-sidecar/lookup/v1\0",
                contract,
                endpoint,
                profile_scope,
            )?,
            io_lock: Arc::new(Mutex::new(())),
        };
        {
            let _guard = store
                .io_lock
                .lock()
                .map_err(|_| "thinking continuity store lock is unavailable".to_string())?;
            let _ = store.cleanup_locked(0, None)?;
        }
        Ok(store)
    }

    pub(crate) fn restore_request(
        &self,
        request: &mut Value,
        model: &str,
        policy: RestorePolicy,
    ) -> Result<(), String> {
        validate_model(model)?;
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| "thinking continuity store lock is unavailable".to_string())?;
        validate_private_root_handle(&self.directory)?;
        let original = request
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| "thinking continuity request messages are invalid".to_string())?;
        let mut messages = original.clone();

        for index in 0..messages.len() {
            if messages[index].get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let already_present = message_has_reasoning(&messages[index])?;
            let needs_restore = match policy {
                RestorePolicy::DeepSeekToolUse => message_has_tool_use(&messages[index])?,
            };
            if !needs_restore || already_present {
                continue;
            }

            let fingerprint = self.history_fingerprint(&messages, index, model)?;
            let target = self.entry_name(&fingerprint);
            let encrypted = read_private_file(&self.directory, &self.root, &target)?
                .ok_or_else(|| "thinking continuity state is missing".to_string())?;
            let stored = self.decrypt(&encrypted)?;
            validate_stored_entry(&stored, &fingerprint, model, policy)?;
            let tools = tool_bindings(&messages[index])?;
            if stored.tools != tools {
                return Err("thinking continuity tool binding changed".into());
            }
            if !stored.no_reasoning {
                insert_reasoning_blocks(&mut messages[index], stored.blocks)?;
            }
        }
        request["messages"] = Value::Array(messages);
        Ok(())
    }

    pub(crate) fn capture_message(
        &self,
        request: &Value,
        response: &Value,
        model: &str,
        policy: RestorePolicy,
    ) -> Result<Option<PendingCommit>, String> {
        validate_model(model)?;
        validate_complete_response(response)?;
        let response_content = response
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| "thinking continuity response content is invalid".to_string())?;
        let (blocks, visible_content) = split_reasoning_blocks(response_content)?;
        let must_capture = match policy {
            RestorePolicy::DeepSeekToolUse => visible_content.iter().any(is_tool_binding_block),
        };
        if !must_capture {
            return Ok(None);
        }
        let no_reasoning = blocks.is_empty()
            && policy == RestorePolicy::DeepSeekToolUse
            && request_has_explicit_no_reasoning(request);
        if blocks.is_empty() && !no_reasoning {
            return Err("provider response omitted required thinking continuity".into());
        }

        let mut history = request
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "thinking continuity request messages are invalid".to_string())?;
        let mut assistant = Map::new();
        assistant.insert("role".into(), Value::String("assistant".into()));
        assistant.insert("content".into(), Value::Array(visible_content));
        history.push(Value::Object(assistant));
        let tools = tool_bindings(history.last().expect("assistant history was appended"))?;
        let fingerprint = self.history_fingerprint(&history, history.len() - 1, model)?;
        let stored = StoredEntry {
            version: 1,
            created_at: now_seconds(),
            fingerprint: fingerprint.clone(),
            model: model.to_string(),
            policy,
            tools,
            no_reasoning,
            blocks,
        };
        let plaintext = Zeroizing::new(
            serde_json::to_vec(&stored)
                .map_err(|_| "thinking continuity entry serialization failed".to_string())?,
        );
        let max_plaintext = MAX_ENTRY_BYTES
            .checked_sub(MAGIC.len() + NONCE_BYTES + CHACHA20_POLY1305.tag_len())
            .ok_or_else(|| "thinking continuity entry size contract is invalid".to_string())?;
        if plaintext.len() > max_plaintext {
            return Err("thinking continuity entry exceeds the size limit".into());
        }
        let encrypted = self.encrypt(plaintext)?;
        if encrypted.len() > MAX_ENTRY_BYTES {
            return Err("thinking continuity entry exceeds the size limit".into());
        }
        Ok(Some(PendingCommit {
            target: self.entry_name(&fingerprint),
            encrypted,
        }))
    }

    pub(crate) fn commit(&self, pending: PendingCommit) -> Result<CommittedEntry, String> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| "thinking continuity store lock is unavailable".to_string())?;
        validate_private_root_handle(&self.directory)?;
        if !valid_entry_name(&pending.target) {
            return Err("thinking continuity commit target is invalid".into());
        }
        let evicted = self.cleanup_locked(pending.encrypted.len() as u64, Some(&pending.target))?;
        let previous = match read_private_file(&self.directory, &self.root, &pending.target) {
            Ok(previous) => previous,
            Err(error) => {
                restore_evicted_entries(
                    &self.directory,
                    &self.root,
                    evicted,
                    "thinking continuity eviction recovery failed",
                )?;
                return Err(error);
            }
        };
        if let Err(error) = atomic_replace(
            &self.directory,
            &self.root,
            &pending.target,
            &pending.encrypted,
        ) {
            restore_evicted_entries(
                &self.directory,
                &self.root,
                evicted,
                "thinking continuity eviction recovery failed",
            )?;
            return Err(error);
        }
        Ok(CommittedEntry {
            root: self.root.clone(),
            directory: Arc::clone(&self.directory),
            target: pending.target,
            previous,
            evicted,
            written_digest: digest(&pending.encrypted),
            io_lock: Arc::clone(&self.io_lock),
        })
    }

    fn entry_name(&self, fingerprint: &str) -> String {
        format!("{fingerprint}.rsn")
    }

    fn history_fingerprint(
        &self,
        messages: &[Value],
        end: usize,
        model: &str,
    ) -> Result<String, String> {
        let bytes = visible_history_bytes(messages, end, model)?;
        let mut mac = HmacSha256::new_from_slice(&self.lookup_key)
            .map_err(|_| "thinking continuity fingerprint key is unavailable".to_string())?;
        mac.update(b"csswitch/anthropic-reasoning-sidecar/fingerprint/v1\0");
        update_hmac_field(&mut mac, &bytes);
        Ok(hex_bytes(&mac.finalize().into_bytes()))
    }

    fn encrypt(&self, mut plaintext: Zeroizing<Vec<u8>>) -> Result<Vec<u8>, String> {
        let mut nonce = [0_u8; NONCE_BYTES];
        getrandom(&mut nonce)
            .map_err(|_| "thinking continuity nonce generation failed".to_string())?;
        let key = LessSafeKey::new(
            UnboundKey::new(&CHACHA20_POLY1305, &self.encryption_key)
                .map_err(|_| "thinking continuity encryption key is invalid".to_string())?,
        );
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(MAGIC),
            &mut *plaintext,
        )
        .map_err(|_| "thinking continuity encryption failed".to_string())?;
        let mut output = Vec::with_capacity(MAGIC.len() + NONCE_BYTES + plaintext.len());
        output.extend_from_slice(MAGIC);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&plaintext);
        Ok(output)
    }

    fn decrypt(&self, encrypted: &[u8]) -> Result<StoredEntry, String> {
        if encrypted.len() > MAX_ENTRY_BYTES
            || encrypted.len() < MAGIC.len() + NONCE_BYTES + CHACHA20_POLY1305.tag_len()
            || !encrypted.starts_with(MAGIC)
        {
            return Err("thinking continuity entry is invalid".into());
        }
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&encrypted[MAGIC.len()..MAGIC.len() + NONCE_BYTES]);
        let mut ciphertext = Zeroizing::new(encrypted[MAGIC.len() + NONCE_BYTES..].to_vec());
        let key = LessSafeKey::new(
            UnboundKey::new(&CHACHA20_POLY1305, &self.encryption_key)
                .map_err(|_| "thinking continuity encryption key is invalid".to_string())?,
        );
        let plaintext = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(MAGIC),
                &mut ciphertext,
            )
            .map_err(|_| "thinking continuity entry authentication failed".to_string())?;
        serde_json::from_slice(plaintext)
            .map_err(|_| "thinking continuity entry payload is invalid".to_string())
    }

    fn cleanup_locked(
        &self,
        incoming: u64,
        target: Option<&str>,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        if incoming > MAX_ENTRY_BYTES as u64 {
            return Err("thinking continuity entry exceeds the size limit".into());
        }
        let mut entries = Vec::new();
        for name in list_directory_names(&self.directory, &self.root)? {
            if valid_temp_name(&name) {
                let file = open_private_file_handle(&self.directory, &self.root, &name)?
                    .ok_or_else(|| "thinking continuity temporary entry vanished".to_string())?;
                let metadata = file
                    .metadata()
                    .map_err(|_| "thinking continuity temporary entry is invalid".to_string())?;
                validate_private_file_metadata(&metadata)?;
                unlink_private_file(&self.directory, &self.root, &name).map_err(|_| {
                    "thinking continuity temporary entry cleanup failed".to_string()
                })?;
                continue;
            }
            if !valid_entry_name(&name) {
                continue;
            }
            let file = open_private_file_handle(&self.directory, &self.root, &name)?
                .ok_or_else(|| "thinking continuity store entry vanished".to_string())?;
            let metadata = file
                .metadata()
                .map_err(|_| "thinking continuity store entry is invalid".to_string())?;
            validate_private_file_metadata(&metadata)?;
            if metadata.len() > MAX_ENTRY_BYTES as u64 {
                return Err("thinking continuity store entry exceeds the size limit".into());
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            if SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > ENTRY_TTL)
            {
                unlink_private_file(&self.directory, &self.root, &name)
                    .map_err(|_| "thinking continuity expired entry cleanup failed".to_string())?;
                continue;
            }
            entries.push(EntryInfo {
                name,
                size: metadata.len(),
                modified,
            });
        }

        let old_target =
            target.and_then(|target| entries.iter().find(|entry| entry.name == target));
        let old_target_size = old_target.map(|entry| entry.size).unwrap_or(0);
        let target_exists = old_target.is_some();
        let total = entries
            .iter()
            .map(|entry| entry.size)
            .sum::<u64>()
            .saturating_sub(old_target_size)
            .saturating_add(incoming);
        let count = entries.len() + usize::from(incoming > 0 && !target_exists);
        let mut planned = Vec::new();
        for name in plan_evictions(&entries, total, count, target, MAX_TOTAL_BYTES, MAX_ENTRIES)? {
            let bytes = read_private_file(&self.directory, &self.root, &name)?
                .ok_or_else(|| "thinking continuity eviction target vanished".to_string())?;
            planned.push((name, bytes));
        }
        let mut evicted = Vec::with_capacity(planned.len());
        for (name, bytes) in planned {
            if unlink_private_file(&self.directory, &self.root, &name).is_err() {
                restore_evicted_entries(
                    &self.directory,
                    &self.root,
                    evicted,
                    "thinking continuity eviction recovery failed",
                )?;
                return Err("thinking continuity bounded cleanup failed".into());
            }
            evicted.push((name, bytes));
        }
        if let Err(error) = sync_directory(&self.directory) {
            restore_evicted_entries(
                &self.directory,
                &self.root,
                evicted,
                "thinking continuity eviction recovery failed",
            )?;
            return Err(error);
        }
        Ok(evicted)
    }
}

fn restore_evicted_entries(
    directory: &File,
    root: &Path,
    evicted: Vec<(String, Vec<u8>)>,
    error: &str,
) -> Result<(), String> {
    for (name, bytes) in evicted {
        if read_private_file(directory, root, &name)?.is_some() {
            return Err(error.into());
        }
        atomic_replace(directory, root, &name, &bytes).map_err(|_| error.to_string())?;
    }
    Ok(())
}

struct EntryInfo {
    name: String,
    size: u64,
    modified: SystemTime,
}

fn plan_evictions(
    entries: &[EntryInfo],
    mut total: u64,
    mut count: usize,
    target: Option<&str>,
    max_total: u64,
    max_entries: usize,
) -> Result<Vec<String>, String> {
    let mut oldest = entries.iter().collect::<Vec<_>>();
    oldest.sort_by_key(|entry| entry.modified);
    let mut remove = Vec::new();
    for entry in oldest {
        if total <= max_total && count <= max_entries {
            break;
        }
        if target == Some(entry.name.as_str()) {
            continue;
        }
        total = total.saturating_sub(entry.size);
        count = count.saturating_sub(1);
        remove.push(entry.name.clone());
    }
    if total > max_total || count > max_entries {
        return Err("thinking continuity store capacity is exhausted".into());
    }
    Ok(remove)
}

fn validate_stored_entry(
    stored: &StoredEntry,
    fingerprint: &str,
    model: &str,
    policy: RestorePolicy,
) -> Result<(), String> {
    let invalid_payload = if stored.no_reasoning {
        stored.policy != RestorePolicy::DeepSeekToolUse
            || stored.tools.is_empty()
            || !stored.blocks.is_empty()
    } else {
        stored.blocks.is_empty()
    };
    if stored.version != 1
        || stored.fingerprint != fingerprint
        || stored.model != model
        || stored.policy != policy
        || stored.tools.len() > MAX_REASONING_BLOCKS
        || invalid_payload
        || stored.blocks.len() > MAX_REASONING_BLOCKS
        || now_seconds().saturating_sub(stored.created_at) > ENTRY_TTL.as_secs()
    {
        return Err("thinking continuity entry scope changed".into());
    }
    for block in &stored.blocks {
        validate_reasoning_block(&block.block)?;
    }
    Ok(())
}

fn request_has_explicit_no_reasoning(request: &Value) -> bool {
    request
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "disabled" | "none"))
}

fn validate_complete_response(response: &Value) -> Result<(), String> {
    let object = response
        .as_object()
        .ok_or_else(|| "thinking continuity response is not a message".to_string())?;
    if object.get("type").and_then(Value::as_str) != Some("message")
        || object.get("role").and_then(Value::as_str) != Some("assistant")
        || !valid_bounded_string(object.get("id"), MAX_ID_BYTES)
        || !valid_bounded_string(object.get("model"), MAX_MODEL_BYTES)
        || !valid_bounded_string(object.get("stop_reason"), MAX_ID_BYTES)
        || !object.get("usage").is_some_and(Value::is_object)
    {
        return Err("thinking continuity response is incomplete".into());
    }
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "thinking continuity response content is invalid".to_string())?;
    if content.is_empty() || content.len() > MAX_CONTENT_BLOCKS {
        return Err("thinking continuity response content is invalid".into());
    }
    Ok(())
}

fn valid_bounded_string(value: Option<&Value>, max: usize) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| {
        !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
    })
}

fn split_reasoning_blocks(content: &[Value]) -> Result<(Vec<StoredBlock>, Vec<Value>), String> {
    let mut blocks = Vec::new();
    let mut visible = Vec::with_capacity(content.len());
    let mut reasoning_bytes = 0_usize;
    for block in content {
        let object = block
            .as_object()
            .ok_or_else(|| "thinking continuity content block is invalid".to_string())?;
        if !valid_bounded_string(object.get("type"), MAX_ID_BYTES) {
            return Err("thinking continuity content block is invalid".into());
        }
        if is_reasoning_block(block) {
            if blocks.len() >= MAX_REASONING_BLOCKS {
                return Err("thinking continuity response has too many reasoning blocks".into());
            }
            validate_reasoning_block(block)?;
            reasoning_bytes = reasoning_bytes
                .checked_add(
                    serde_json::to_vec(block)
                        .map_err(|_| "thinking continuity reasoning block is invalid".to_string())?
                        .len(),
                )
                .filter(|total| *total <= MAX_ENTRY_BYTES)
                .ok_or_else(|| {
                    "thinking continuity reasoning blocks exceed the size limit".to_string()
                })?;
            let index = blocks.len() + visible.len();
            blocks.push(StoredBlock {
                index,
                block: block.clone(),
            });
        } else {
            visible.push(block.clone());
        }
    }
    Ok((blocks, visible))
}

fn validate_reasoning_block(block: &Value) -> Result<(), String> {
    let object = block
        .as_object()
        .ok_or_else(|| "thinking continuity reasoning block is invalid".to_string())?;
    match object.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            if object
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "thinking" | "signature"))
            {
                return Err("thinking continuity reasoning block has unexpected fields".into());
            }
            let thinking = object
                .get("thinking")
                .and_then(Value::as_str)
                .ok_or_else(|| "thinking continuity reasoning content is invalid".to_string())?;
            let signature = object
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if thinking.len().saturating_add(signature.len()) > MAX_ENTRY_BYTES
                || (thinking.is_empty() && signature.is_empty())
            {
                return Err("thinking continuity reasoning content is invalid".into());
            }
        }
        Some("redacted_thinking") => {
            if object
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "data"))
                || !object
                    .get("data")
                    .and_then(Value::as_str)
                    .is_some_and(|data| !data.is_empty() && data.len() <= MAX_ENTRY_BYTES)
            {
                return Err("thinking continuity redacted reasoning is invalid".into());
            }
        }
        _ => return Err("thinking continuity reasoning block is invalid".into()),
    }
    Ok(())
}

fn insert_reasoning_blocks(message: &mut Value, blocks: Vec<StoredBlock>) -> Result<(), String> {
    let content_value = message
        .get_mut("content")
        .ok_or_else(|| "thinking continuity assistant content is invalid".to_string())?;
    if let Value::String(text) = content_value {
        let text = std::mem::take(text);
        *content_value = Value::Array(vec![serde_json::json!({"type": "text", "text": text})]);
    }
    let content = content_value
        .as_array_mut()
        .ok_or_else(|| "thinking continuity assistant content is invalid".to_string())?;
    let mut previous = None;
    for stored in blocks {
        if previous.is_some_and(|index| stored.index <= index)
            || stored.index > content.len()
            || !is_reasoning_block(&stored.block)
        {
            return Err("thinking continuity block position is invalid".into());
        }
        previous = Some(stored.index);
        content.insert(stored.index, stored.block);
    }
    Ok(())
}

fn message_has_reasoning(message: &Value) -> Result<bool, String> {
    let Some(content) = message.get("content") else {
        return Err("thinking continuity assistant content is missing".into());
    };
    match content {
        Value::String(_) => Ok(false),
        Value::Array(blocks) => Ok(blocks.iter().any(is_reasoning_block)),
        _ => Err("thinking continuity assistant content is invalid".into()),
    }
}

fn message_has_tool_use(message: &Value) -> Result<bool, String> {
    let Some(content) = message.get("content") else {
        return Err("thinking continuity assistant content is missing".into());
    };
    match content {
        Value::String(_) => Ok(false),
        Value::Array(blocks) => Ok(blocks.iter().any(is_tool_binding_block)),
        _ => Err("thinking continuity assistant content is invalid".into()),
    }
}

fn is_reasoning_block(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    )
}

fn is_tool_binding_block(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("tool_use" | "server_tool_use")
    )
}

fn tool_bindings(message: &Value) -> Result<Vec<ToolBinding>, String> {
    let Some(content) = message.get("content") else {
        return Err("thinking continuity assistant content is missing".into());
    };
    let Value::Array(blocks) = content else {
        return Ok(Vec::new());
    };
    let mut tools = Vec::new();
    for block in blocks.iter().filter(|block| is_tool_binding_block(block)) {
        if tools.len() >= MAX_REASONING_BLOCKS {
            return Err("thinking continuity response has too many tool bindings".into());
        }
        let object = block
            .as_object()
            .ok_or_else(|| "thinking continuity tool binding is invalid".to_string())?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
            })
            .ok_or_else(|| "thinking continuity tool id is invalid".to_string())?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= MAX_TOOL_NAME_BYTES
                    && !name.chars().any(char::is_control)
            })
            .ok_or_else(|| "thinking continuity tool name is invalid".to_string())?;
        let input = object
            .get("input")
            .ok_or_else(|| "thinking continuity tool input is missing".to_string())?;
        let encoded = serde_json::to_vec(&canonicalize(input))
            .map_err(|_| "thinking continuity tool input is invalid".to_string())?;
        if encoded.len() > MAX_ENTRY_BYTES {
            return Err("thinking continuity tool input is too large".into());
        }
        tools.push(ToolBinding {
            id: id.to_string(),
            name: name.to_string(),
            input_digest: hex_bytes(&digest(&encoded)),
        });
    }
    Ok(tools)
}

fn visible_history_bytes(messages: &[Value], end: usize, model: &str) -> Result<Vec<u8>, String> {
    if end >= messages.len() {
        return Err("thinking continuity history prefix is invalid".into());
    }
    let mut prefix = Vec::with_capacity(end + 1);
    for message in &messages[..=end] {
        let mut visible = message.clone();
        let is_assistant = visible.get("role").and_then(Value::as_str) == Some("assistant");
        if let Some(content) = visible.get_mut("content") {
            if let Value::String(text) = content {
                let text = std::mem::take(text);
                *content = Value::Array(vec![serde_json::json!({
                    "type": "text",
                    "text": text,
                })]);
            }
            if is_assistant {
                let blocks = content.as_array_mut().ok_or_else(|| {
                    "thinking continuity assistant content is invalid".to_string()
                })?;
                blocks.retain(|block| !is_reasoning_block(block));
            }
        } else if is_assistant {
            return Err("thinking continuity assistant content is missing".into());
        }
        normalize_history_for_fingerprint(&mut visible);
        prefix.push(visible);
    }
    let mut envelope = Map::new();
    envelope.insert("model".into(), Value::String(model.to_string()));
    envelope.insert("messages".into(), Value::Array(prefix));
    let canonical = canonicalize(&Value::Object(envelope));
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|_| "thinking continuity fingerprint serialization failed".to_string())?;
    if bytes.len() > MAX_FINGERPRINT_INPUT_BYTES {
        return Err("thinking continuity history prefix is too large".into());
    }
    Ok(bytes)
}

fn normalize_history_for_fingerprint(value: &mut Value) {
    match value {
        Value::Array(values) => values
            .iter_mut()
            .for_each(normalize_history_for_fingerprint),
        Value::Object(values) => {
            values.remove("cache_control");
            if values.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(Value::String(text)) = values.get_mut("content") {
                    let text = std::mem::take(text);
                    *values
                        .get_mut("content")
                        .expect("tool result content exists") =
                        Value::Array(vec![serde_json::json!({
                            "type": "text",
                            "text": text,
                        })]);
                }
            }
            values
                .values_mut()
                .for_each(normalize_history_for_fingerprint);
        }
        _ => {}
    }
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut ordered = Map::new();
            for key in keys {
                ordered.insert(key.clone(), canonicalize(&values[key]));
            }
            Value::Object(ordered)
        }
        _ => value.clone(),
    }
}

fn validate_model(model: &str) -> Result<(), String> {
    if model.is_empty() || model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control) {
        return Err("thinking continuity model is invalid".into());
    }
    Ok(())
}

fn valid_entry_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 68
        && &bytes[64..] == b".rsn"
        && bytes[..64]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_temp_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 37
        && bytes[0] == b'.'
        && &bytes[33..] == b".tmp"
        && bytes[1..33]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn open_private_root(root: &Path) -> Result<File, String> {
    if !root.is_absolute()
        || root == Path::new("/")
        || root.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err("thinking continuity store root is invalid".into());
    }
    let canonical = fs::canonicalize(root)
        .map_err(|_| "thinking continuity store root is unavailable".to_string())?;
    if canonical != root {
        return Err("thinking continuity store root is unsafe".into());
    }
    let path_metadata = fs::symlink_metadata(root)
        .map_err(|_| "thinking continuity store root is unavailable".to_string())?;
    validate_private_root_metadata(&path_metadata)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let directory = options
        .open(root)
        .map_err(|_| "thinking continuity store root cannot be opened".to_string())?;
    let handle_metadata = directory
        .metadata()
        .map_err(|_| "thinking continuity store root metadata is unavailable".to_string())?;
    validate_private_root_metadata(&handle_metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_metadata.dev() != handle_metadata.dev()
            || path_metadata.ino() != handle_metadata.ino()
        {
            return Err("thinking continuity store root changed while opening".into());
        }
    }
    Ok(directory)
}

fn validate_private_root_handle(directory: &File) -> Result<(), String> {
    let metadata = directory
        .metadata()
        .map_err(|_| "thinking continuity store root metadata is unavailable".to_string())?;
    validate_private_root_metadata(&metadata)
}

fn validate_private_root_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("thinking continuity store root is unsafe".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err("thinking continuity store root permissions are unsafe".into());
        }
    }
    Ok(())
}

fn validate_private_file_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("thinking continuity store entry is unsafe".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err("thinking continuity store entry permissions are unsafe".into());
        }
    }
    Ok(())
}

fn checked_name(name: &str) -> Result<CString, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err("thinking continuity store entry name is invalid".into());
    }
    CString::new(name).map_err(|_| "thinking continuity store entry name is invalid".to_string())
}

fn open_private_file_handle(
    directory: &File,
    _root: &Path,
    name: &str,
) -> Result<Option<File>, String> {
    #[cfg(unix)]
    {
        let name = checked_name(name)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err("thinking continuity store entry cannot be opened".into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Some(file))
    }

    #[cfg(not(unix))]
    let file = match OpenOptions::new().read(true).open(_root.join(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("thinking continuity store entry cannot be opened".into()),
    };
    #[cfg(not(unix))]
    Ok(Some(file))
}

fn read_private_file(directory: &File, root: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    let Some(mut file) = open_private_file_handle(directory, root, name)? else {
        return Ok(None);
    };
    let metadata = file
        .metadata()
        .map_err(|_| "thinking continuity store entry metadata is unavailable".to_string())?;
    validate_private_file_metadata(&metadata)?;
    if metadata.len() > MAX_ENTRY_BYTES as u64 {
        return Err("thinking continuity store entry exceeds the size limit".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_ENTRY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "thinking continuity store entry cannot be read".to_string())?;
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err("thinking continuity store entry exceeds the size limit".into());
    }
    Ok(Some(bytes))
}

fn atomic_replace(directory: &File, root: &Path, target: &str, bytes: &[u8]) -> Result<(), String> {
    if !valid_entry_name(target) {
        return Err("thinking continuity commit target is invalid".into());
    }
    let temp = format!(".{}.tmp", random_hex()?);
    let result: Result<(), String> = (|| {
        #[cfg(unix)]
        let mut file = {
            let temp = checked_name(&temp)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    temp.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err("thinking continuity temporary entry cannot be created".into());
            }
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(not(unix))]
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(&temp))
            .map_err(|_| "thinking continuity temporary entry cannot be created".to_string())?;
        #[cfg(unix)]
        {
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err("thinking continuity temporary entry permissions failed".into());
            }
        }
        validate_private_file_metadata(
            &file
                .metadata()
                .map_err(|_| "thinking continuity temporary entry is invalid".to_string())?,
        )?;
        file.write_all(bytes)
            .map_err(|_| "thinking continuity entry write failed".to_string())?;
        file.sync_all()
            .map_err(|_| "thinking continuity entry sync failed".to_string())?;
        #[cfg(unix)]
        {
            let temp = checked_name(&temp)?;
            let target = checked_name(target)?;
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temp.as_ptr(),
                    directory.as_raw_fd(),
                    target.as_ptr(),
                )
            } != 0
            {
                return Err("thinking continuity entry commit failed".into());
            }
        }
        #[cfg(not(unix))]
        fs::rename(root.join(&temp), root.join(target))
            .map_err(|_| "thinking continuity entry commit failed".to_string())?;
        sync_directory(directory)
    })();
    if result.is_err() {
        let _ = unlink_private_file(directory, root, &temp);
    }
    result
}

fn unlink_private_file(directory: &File, _root: &Path, name: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let name = CString::new(name)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fs::remove_file(_root.join(name))
}

fn sync_directory(directory: &File) -> Result<(), String> {
    directory
        .sync_all()
        .map_err(|_| "thinking continuity store sync failed".to_string())
}

fn list_directory_names(directory: &File, _root: &Path) -> Result<Vec<String>, String> {
    #[cfg(unix)]
    {
        let dot = c".";
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err("thinking continuity store cannot be listed".into());
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe { libc::close(fd) };
            return Err("thinking continuity store cannot be listed".into());
        }
        let result = (|| {
            let mut names = Vec::new();
            loop {
                set_errno(0);
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    if current_errno() != 0 {
                        return Err("thinking continuity store cannot be listed".to_string());
                    }
                    break;
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                    .to_str()
                    .map_err(|_| "thinking continuity store entry name is invalid".to_string())?;
                if name != "." && name != ".." {
                    names.push(name.to_string());
                }
            }
            Ok(names)
        })();
        if unsafe { libc::closedir(stream) } != 0 {
            return Err("thinking continuity store cannot be listed".into());
        }
        result
    }

    #[cfg(not(unix))]
    fs::read_dir(_root)
        .map_err(|_| "thinking continuity store cannot be listed".to_string())?
        .map(|entry| {
            entry
                .map_err(|_| "thinking continuity store entry is invalid".to_string())?
                .file_name()
                .into_string()
                .map_err(|_| "thinking continuity store entry name is invalid".to_string())
        })
        .collect()
}

#[cfg(all(unix, target_os = "macos"))]
fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(unix)]
fn set_errno(value: libc::c_int) {
    unsafe { *errno_pointer() = value };
}

#[cfg(unix)]
fn current_errno() -> libc::c_int {
    unsafe { *errno_pointer() }
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom(&mut bytes)
        .map_err(|_| "thinking continuity temporary name generation failed".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn derive_scope_key(
    credential: &str,
    domain: &[u8],
    contract: &str,
    endpoint: &str,
    profile_scope: &str,
) -> Result<[u8; 32], String> {
    let mut derivation = HmacSha256::new_from_slice(credential.as_bytes())
        .map_err(|_| "thinking continuity key scope is unavailable".to_string())?;
    derivation.update(domain);
    for value in [contract, endpoint, profile_scope] {
        update_hmac_field(&mut derivation, value.as_bytes());
    }
    Ok(derivation.finalize().into_bytes().into())
}

fn update_hmac_field(mac: &mut HmacSha256, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{ReasoningStore, RestorePolicy, MAX_ENTRIES, MAX_ENTRY_BYTES};
    use serde_json::{json, Map, Value};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "csswitch-reasoning-state-{}-{}",
                std::process::id(),
                NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(fs::canonicalize(path).unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn entry(&self) -> PathBuf {
            fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().and_then(|value| value.to_str()) == Some("rsn"))
                .unwrap()
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store(root: &TestRoot, scope: &str) -> ReasoningStore {
        ReasoningStore::open(
            root.path(),
            "test-provider-credential",
            "kimi-anthropic-relay",
            "https://provider.invalid/v1/messages",
            scope,
        )
        .unwrap()
    }

    fn first_request() -> Value {
        json!({"messages": [{"role": "user", "content": "hello"}]})
    }

    fn complete_response(content: Value, stop_reason: &str) -> Value {
        json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "provider-model",
            "content": content,
            "stop_reason": stop_reason,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
    }

    #[test]
    fn store_round_trip_survives_restart_and_key_order_changes() {
        let root = TestRoot::new();
        let request = first_request();
        let response = complete_response(
            json!([
                {"type": "thinking", "thinking": "provider-only", "signature": "opaque"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}}
            ]),
            "tool_use",
        );
        let first = store(&root, "profile-a");
        let pending = first
            .capture_message(&request, &response, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap()
            .unwrap();
        first.commit(pending).unwrap();
        let entry = root.entry();
        let encrypted = fs::read(&entry).unwrap();
        assert!(!encrypted
            .windows(b"provider-only".len())
            .any(|window| window == b"provider-only"));
        assert!(!format!("{first:?}").contains(root.path().to_str().unwrap()));

        // Rebuild the tool_use block with the keys inserted in a different
        // order: the fingerprint is semantic, so it must still match.
        let mut tool_use = Map::new();
        tool_use.insert("input".into(), json!({"code": "1+1"}));
        tool_use.insert("name".into(), Value::String("python".into()));
        tool_use.insert("id".into(), Value::String("toolu_1".into()));
        tool_use.insert("type".into(), Value::String("tool_use".into()));
        let mut next = json!({
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": []},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "2"}
                ]}
            ]
        });
        next["messages"][1]["content"] = Value::Array(vec![Value::Object(tool_use)]);
        drop(first);

        let restarted = store(&root, "profile-a");
        restarted
            .restore_request(&mut next, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(
            next["messages"][1]["content"][0]["thinking"],
            "provider-only"
        );
        assert_eq!(next["messages"][1]["content"][1]["id"], "toolu_1");
    }

    #[test]
    fn semantic_history_fingerprint_survives_two_rounds_and_cache_control_movement() {
        let root = TestRoot::new();
        let store = store(&root, "profile-semantic-history");
        let first_request = json!({"messages": [
            {"role": "user", "content": "hello"}
        ]});
        let first_response = complete_response(
            json!([
                {"type": "thinking", "thinking": "first-plan", "signature": "first-sig"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1"}}
            ]),
            "tool_use",
        );
        let first_pending = store
            .capture_message(
                &first_request,
                &first_response,
                "k3",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        store.commit(first_pending).unwrap();

        // cache_control markers move between turns as the client re-anchors its
        // prompt cache; the fingerprint must ignore them.
        let mut second_request = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "1"}
            ]}
        ]});
        store
            .restore_request(&mut second_request, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(
            second_request["messages"][1]["content"][0]["thinking"],
            "first-plan"
        );

        let second_response = complete_response(
            json!([
                {"type": "thinking", "thinking": "second-plan", "signature": "second-sig"},
                {"type": "tool_use", "id": "toolu_2", "name": "python", "input": {"code": "2"}}
            ]),
            "tool_use",
        );
        let second_pending = store
            .capture_message(
                &second_request,
                &second_response,
                "k3",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        store.commit(second_pending).unwrap();

        let third_template = json!({
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1"}, "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "1", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_2", "name": "python", "input": {"code": "2"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "2"}
                ]}
            ]
        });
        let mut third_request = third_template.clone();
        store
            .restore_request(&mut third_request, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(
            third_request["messages"][1]["content"][0]["thinking"],
            "first-plan"
        );
        assert_eq!(
            third_request["messages"][3]["content"][0]["thinking"],
            "second-plan"
        );

        // A genuine history edit must fail closed rather than replay stale state.
        let mut changed_tool = third_template;
        changed_tool["messages"][1]["content"][0]["input"] = json!({"code": "changed"});
        assert!(store
            .restore_request(&mut changed_tool, "k3", RestorePolicy::DeepSeekToolUse)
            .is_err());
    }

    #[test]
    fn deepseek_restores_only_tool_use_reasoning_and_rollback_removes_new_entry() {
        let root = TestRoot::new();
        let store = ReasoningStore::open(
            root.path(),
            "deepseek-key",
            "deepseek-native",
            "https://api.deepseek.invalid/anthropic/v1/messages",
            "profile-ds",
        )
        .unwrap();
        let request = first_request();
        let pure = complete_response(
            json!([
                {"type": "thinking", "thinking": "pure", "signature": "sig"},
                {"type": "text", "text": "answer"}
            ]),
            "end_turn",
        );
        assert!(store
            .capture_message(
                &request,
                &pure,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse
            )
            .unwrap()
            .is_none());

        let tool = complete_response(
            json!([
                {"type": "thinking", "thinking": "tool-plan", "signature": "sig"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}}
            ]),
            "tool_use",
        );
        let pending = store
            .capture_message(
                &request,
                &tool,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        let committed = store.commit(pending).unwrap();
        let entry = root.entry();
        let mut next = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}, "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "2", "cache_control": {"type": "ephemeral"}}
            ]}
        ]});
        store
            .restore_request(&mut next, "deepseek-v4-pro", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(next["messages"][1]["content"][0]["thinking"], "tool-plan");

        let second_tool = complete_response(
            json!([
                {"type": "thinking", "thinking": "second-tool-plan", "signature": "sig-2"},
                {"type": "tool_use", "id": "toolu_2", "name": "search_skills", "input": {"query": "pubmed"}}
            ]),
            "tool_use",
        );
        let second_pending = store
            .capture_message(
                &next,
                &second_tool,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        let second_committed = store.commit(second_pending).unwrap();
        let mut chained = next.clone();
        chained["messages"][1]["content"] = json!([
            {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}}
        ]);
        chained["messages"][2]["content"][0]["content"] = json!([
            {"type": "text", "text": "2"}
        ]);
        chained["messages"].as_array_mut().unwrap().extend([
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_2", "name": "search_skills", "input": {"query": "pubmed"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": "found"}
            ]}),
        ]);
        let mut changed_result = chained.clone();
        changed_result["messages"][2]["content"][0]["content"][0]["text"] = json!("3");
        assert!(store
            .restore_request(
                &mut changed_result,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .is_err());
        store
            .restore_request(
                &mut chained,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap();
        assert_eq!(
            chained["messages"][1]["content"][0]["thinking"],
            "tool-plan"
        );
        assert_eq!(
            chained["messages"][3]["content"][0]["thinking"],
            "second-tool-plan"
        );
        second_committed.rollback().unwrap();
        committed.rollback().unwrap();
        assert!(!entry.exists());

        let search = complete_response(
            json!([
                {"type": "thinking", "thinking": "search-plan", "signature": "search-sig"},
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "x"}},
                {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": []},
                {"type": "text", "text": "search answer"}
            ]),
            "end_turn",
        );
        let pending = store
            .capture_message(
                &request,
                &search,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        store.commit(pending).unwrap();
        let mut next = json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "x"}},
                {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": []},
                {"type": "text", "text": "search answer"}
            ]},
            {"role": "user", "content": "follow up"}
        ]});
        store
            .restore_request(&mut next, "deepseek-v4-pro", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(next["messages"][1]["content"][0]["thinking"], "search-plan");
    }

    #[test]
    fn deepseek_disabled_tool_use_tombstone_preserves_explicit_no_reasoning() {
        let root = TestRoot::new();
        let store = ReasoningStore::open(
            root.path(),
            "deepseek-key",
            "deepseek-native",
            "https://api.deepseek.invalid/anthropic/v1/messages",
            "profile-disabled-tool",
        )
        .unwrap();
        let request = json!({
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content": "calculate"}],
        });
        let response = complete_response(
            json!([{
                "type": "tool_use",
                "id": "toolu_disabled_python",
                "name": "python",
                "input": {"code": "1+1"},
            }]),
            "tool_use",
        );
        let pending = store
            .capture_message(
                &request,
                &response,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        store.commit(pending).unwrap();
        let encrypted = fs::read(root.entry()).unwrap();
        assert!(!encrypted
            .windows(b"toolu_disabled_python".len())
            .any(|window| window == b"toolu_disabled_python"));

        let mut follow_up = json!({
            "thinking": {"type": "auto"},
            "messages": [
                {"role": "user", "content": "calculate"},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_disabled_python",
                    "name": "python",
                    "input": {"code": "1+1"},
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_disabled_python",
                    "content": "2",
                }]},
            ],
        });
        store
            .restore_request(
                &mut follow_up,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap();
        assert_eq!(
            follow_up["messages"][1]["content"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(follow_up["messages"][1]["content"][0]["type"], "tool_use");

        let mut changed_tool = follow_up.clone();
        changed_tool["messages"][1]["content"][0]["input"] = json!({"code": "2+2"});
        assert!(store
            .restore_request(
                &mut changed_tool,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .is_err());

        let enabled_request = json!({
            "thinking": {"type": "auto"},
            "messages": [{"role": "user", "content": "calculate"}],
        });
        assert!(store
            .capture_message(
                &enabled_request,
                &response,
                "deepseek-v4-pro",
                RestorePolicy::DeepSeekToolUse,
            )
            .is_err());
    }

    #[test]
    fn tamper_scope_model_and_tool_replay_fail_closed() {
        let root = TestRoot::new();
        let request = first_request();
        let response = complete_response(
            json!([
                {"type": "thinking", "thinking": "secret", "signature": "sig"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}}
            ]),
            "tool_use",
        );
        let first = store(&root, "profile-a");
        let first_pending = first
            .capture_message(&request, &response, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap()
            .unwrap();
        let first_target = first_pending.target.clone();
        first.commit(first_pending).unwrap();
        let clean_history = json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}}
            ]}
        ]});

        let mut wrong_model = clean_history.clone();
        assert!(first
            .restore_request(&mut wrong_model, "k3-256k", RestorePolicy::DeepSeekToolUse)
            .is_err());
        let mut wrong_tool = clean_history.clone();
        wrong_tool["messages"][1]["content"][0]["input"] = json!({"code": "2+2"});
        assert!(first
            .restore_request(&mut wrong_tool, "k3", RestorePolicy::DeepSeekToolUse)
            .is_err());
        drop(first);

        let changed_scope = store(&root, "profile-b");
        let mut scoped = clean_history.clone();
        assert!(changed_scope
            .restore_request(&mut scoped, "k3", RestorePolicy::DeepSeekToolUse)
            .is_err());
        let second_response = complete_response(
            json!([
                {"type": "thinking", "thinking": "other-secret", "signature": "other-sig"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1+1"}}
            ]),
            "tool_use",
        );
        let second_pending = changed_scope
            .capture_message(
                &request,
                &second_response,
                "k3",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        assert_ne!(first_target, second_pending.target);
        changed_scope.commit(second_pending).unwrap();
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("rsn")
                })
                .count(),
            2
        );
        let mut scoped = clean_history.clone();
        changed_scope
            .restore_request(&mut scoped, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(
            scoped["messages"][1]["content"][0]["thinking"],
            "other-secret"
        );
        drop(changed_scope);

        let reopened = store(&root, "profile-a");
        let mut original_scope = clean_history.clone();
        reopened
            .restore_request(&mut original_scope, "k3", RestorePolicy::DeepSeekToolUse)
            .unwrap();
        assert_eq!(
            original_scope["messages"][1]["content"][0]["thinking"],
            "secret"
        );
        drop(reopened);

        let entry = root.path().join(first_target);
        let mut bytes = fs::read(&entry).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        fs::write(&entry, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&entry, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let reopened = store(&root, "profile-a");
        let mut tampered = clean_history;
        assert!(reopened
            .restore_request(&mut tampered, "k3", RestorePolicy::DeepSeekToolUse)
            .is_err());
    }

    #[test]
    fn unsafe_roots_entries_and_oversized_state_are_rejected() {
        let root = TestRoot::new();
        let bounded = store(&root, "bounded-profile");
        let half = "x".repeat(MAX_ENTRY_BYTES / 2);
        let response = complete_response(
            json!([
                {"type": "thinking", "thinking": half, "signature": "sig-a"},
                {"type": "thinking", "thinking": "x".repeat(MAX_ENTRY_BYTES / 2), "signature": "sig-b"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1"}}
            ]),
            "tool_use",
        );
        assert!(bounded
            .capture_message(
                &first_request(),
                &response,
                "k3",
                RestorePolicy::DeepSeekToolUse
            )
            .is_err());
        drop(bounded);

        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let link = root.path().with_extension("link");
            symlink(root.path(), &link).unwrap();
            assert!(ReasoningStore::open(
                &link,
                "key",
                "contract",
                "https://endpoint.invalid",
                "scope"
            )
            .is_err());
            fs::remove_file(link).unwrap();

            let outside = root.path().with_extension("outside");
            fs::write(&outside, b"sentinel").unwrap();
            fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
            let entry_link = root.path().join(format!("{}.rsn", "b".repeat(64)));
            symlink(&outside, &entry_link).unwrap();
            assert!(ReasoningStore::open(
                root.path(),
                "key",
                "contract",
                "https://endpoint.invalid",
                "scope"
            )
            .is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"sentinel");
            fs::remove_file(entry_link).unwrap();
            fs::remove_file(outside).unwrap();

            let oversized = root.path().join(format!("{}.rsn", "a".repeat(64)));
            fs::write(&oversized, vec![0_u8; MAX_ENTRY_BYTES + 1]).unwrap();
            fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(ReasoningStore::open(
                root.path(),
                "key",
                "contract",
                "https://endpoint.invalid",
                "scope"
            )
            .is_err());
        }
    }

    #[test]
    fn provisional_commit_rollback_restores_capacity_evictions() {
        let root = TestRoot::new();
        let store = store(&root, "capacity-profile");
        let response = complete_response(
            json!([
                {"type": "thinking", "thinking": "capacity-plan", "signature": "sig"},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1"}}
            ]),
            "tool_use",
        );
        let pending = store
            .capture_message(
                &first_request(),
                &response,
                "k3",
                RestorePolicy::DeepSeekToolUse,
            )
            .unwrap()
            .unwrap();
        let target = pending.target.clone();

        let mut created = 0_usize;
        for index in 0_u64.. {
            if created == MAX_ENTRIES {
                break;
            }
            let name = format!("{index:064x}.rsn");
            if name == target {
                continue;
            }
            let path = root.path().join(name);
            fs::write(&path, format!("victim-{index}")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            created += 1;
        }

        let committed = store.commit(pending).unwrap();
        assert_eq!(committed.evicted.len(), 1);
        let evicted = committed.evicted[0].clone();
        assert!(!root.path().join(&evicted.0).exists());
        committed.rollback().unwrap();

        assert!(!root.path().join(target).exists());
        assert_eq!(fs::read(root.path().join(evicted.0)).unwrap(), evicted.1);
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("rsn")
                })
                .count(),
            MAX_ENTRIES
        );
    }
}
