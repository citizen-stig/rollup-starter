use super::{Runtime, Spec, API_URL};
use futures::stream::BoxStream;
use futures::FutureExt;
use serde::de::DeserializeOwned;
use sov_api_spec::types::Slot;
use sov_modules_api::capabilities::config_chain_id;
use sov_modules_api::transaction::{Transaction, TxDetails};
use sov_modules_api::{DispatchCall, PrivateKey, Runtime as RuntimeTrait};
use sov_rollup_interface::node::ledger_api::IncludeChildren;
use sov_test_utils::{TransactionType, TEST_DEFAULT_MAX_FEE, TEST_DEFAULT_MAX_PRIORITY_FEE};
use std::collections::HashMap;
use tokio::sync::watch;
use tokio_stream::StreamExt;

#[derive(serde::Deserialize, Debug)]
struct StateRootResponse {
    root_hashes: Vec<u8>,
}

#[derive(serde::Deserialize, Debug)]
struct ValueResponse<T> {
    value: T,
}

async fn drain_slot_stream(
    slot_stream: &mut BoxStream<'static, anyhow::Result<Slot>>,
) -> anyhow::Result<Option<Slot>> {
    const MAX_CONSECUTIVE_ERRORS: u32 = 5;
    let mut consecutive_errors = 0;

    let mut latest_slot = loop {
        match slot_stream.next().await {
            Some(Ok(slot)) => {
                break slot;
            }
            Some(Err(e)) => {
                consecutive_errors += 1;
                tracing::error!(
                    "Error receiving slot (attempt {}/{}): {}",
                    consecutive_errors,
                    MAX_CONSECUTIVE_ERRORS,
                    e
                );
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    anyhow::bail!(
                        "Too many consecutive slot stream errors ({}), worker exiting",
                        MAX_CONSECUTIVE_ERRORS
                    );
                }
                continue; // Keep trying until we get a valid slot
            }
            None => return Ok(None), // Stream closed
        }
    };

    tracing::trace!(
        "State validation: got new slot notification, number = {}",
        latest_slot.number
    );

    // Drain any additional queued slots (non-blocking)
    let mut drained_count = 0;
    while let Some(Some(Ok(slot))) = slot_stream.next().now_or_never() {
        latest_slot = slot;
        drained_count += 1;
    }

    if drained_count > 0 {
        tracing::warn!(
            "State validation worker drained {} slots - slots are being produced faster than we can process them! Those slots will have some validation checks skipped.",
            drained_count
        );
    }

    Ok(Some(latest_slot))
}

async fn query_state_value<T: DeserializeOwned>(
    client: &sov_api_spec::Client,
    url: &str,
) -> reqwest::Result<T> {
    client
        .client()
        .get(url)
        .send()
        .await?
        .json::<ValueResponse<T>>()
        .await
        .map(|resp| resp.value)
}

async fn query_visible_slot(client: &sov_api_spec::Client) -> reqwest::Result<u64> {
    let visible_slot_url = format!(
        "{}/modules/state-consistency/state/latest-visible-slot-number/",
        API_URL
    );
    query_state_value::<u64>(&client, &visible_slot_url).await
}

async fn query_state_root(client: &sov_api_spec::Client) -> reqwest::Result<StateRootResponse> {
    let state_root_url = format!(
        "{}/modules/state-consistency/state/latest-state-root/",
        API_URL
    );
    query_state_value::<StateRootResponse>(&client, &state_root_url).await
}

async fn query_rollup_height(client: &sov_api_spec::Client) -> reqwest::Result<u64> {
    let rollup_height_url = format!(
        "{}/modules/state-consistency/state/latest-rollup-height/",
        API_URL
    );
    query_state_value::<u64>(&client, &rollup_height_url).await
}

async fn poll_for_visible_slot_update(
    client: &sov_api_spec::Client,
    slot_number: u64,
    last_visible_slot_number: u64,
) -> anyhow::Result<u64> {
    let max_attempts = 600; // 600 * 10ms = 6s = two full slots at the default config
    let mut attempt = 0;

    loop {
        let visible_slot = query_visible_slot(&client).await?;

        // Check if the API state has caught up to the slot notification
        if visible_slot > last_visible_slot_number {
            tracing::debug!(
                "State consistency: waited {} ms for API state to be updated after receiving new slot...",
                attempt * 10
            );
            return Ok(visible_slot);
        }

        attempt += 1;
        if attempt >= max_attempts {
            anyhow::bail!(
                "State validation worker timed out waiting for API state to update. Slot notification: {slot_number}, expected visible slot: {last_visible_slot_number}, API visible_slot: {visible_slot}. This is either an error in the sequencer, or the acceptance test has a bug."
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

fn create_assert_block_state_tx(
    expected_visible_slot_number: u64,
    expected_rollup_height: u64,
    expected_state_root: Vec<u8>,
) -> anyhow::Result<Transaction<Runtime, Spec>> {
    // Generate a new key for this transaction
    let key = <<Spec as sov_modules_api::Spec>::CryptoSpec as sov_modules_api::CryptoSpec>::PrivateKey::generate();

    let message = <Runtime as DispatchCall>::Decodable::StateConsistency(
        sov_test_state_consistency::CallMessage::AssertBlockState {
            expected_visible_slot_number,
            expected_rollup_height,
            expected_state_root,
        },
    );

    // Sign but DON'T serialize - let send_tx_to_sequencer handle serialization
    Ok(TransactionType::<Runtime, Spec>::sign(
        message,
        key.clone(),
        &Runtime::CHAIN_HASH,
        TxDetails {
            max_priority_fee_bips: TEST_DEFAULT_MAX_PRIORITY_FEE,
            max_fee: TEST_DEFAULT_MAX_FEE,
            gas_limit: None,
            chain_id: config_chain_id(),
        },
        &mut HashMap::from([(key.pub_key(), 0)]),
    ))
}

async fn send_assert_block_tx(
    client: &sov_api_spec::Client,
    visible_slot: u64,
    rollup_height: u64,
    state_root: Vec<u8>,
) -> anyhow::Result<()> {
    let assert_tx = create_assert_block_state_tx(visible_slot, rollup_height, state_root)?;

    match client.send_tx_to_sequencer(&assert_tx).await {
        Ok(_) => Ok(()),
        Err(e) => {
            if e.to_string()
                .contains("The preferred sequencer has reached the stop height")
            {
                tracing::info!(
                    "State validation worker detected sequencer stop height, shutting down"
                );
                return Ok(());
            }

            // Check if this is a "too late" error (actual rollup height > expected)
            if is_too_late_error(&e, rollup_height) {
                tracing::warn!(
                    "State assertion tx for slot {visible_slot} height {rollup_height} was rejected because the rollup already advanced past it. This can happen, but the kernel assertions have been skipped for this slot as a result."
                );
                return Ok(());
            }

            anyhow::bail!(
                "Failed to submit state assertion tx for slot {visible_slot} height {rollup_height}: {e}",
            );
        }
    }
}

/// Check if the error is a "too late" error, meaning the transaction was rejected
/// because the actual rollup height is greater than the expected height.
fn is_too_late_error(
    e: &sov_api_spec::Error<sov_api_spec::types::ApiError>,
    expected_height: u64,
) -> bool {
    // Extract the ApiError from the progenitor Error wrapper
    let api_error = match e {
        sov_api_spec::Error::ErrorResponse(response_value) => response_value,
        _ => return false,
    };

    // Get the error string from the details map
    let error_str = match api_error.details.get("message") {
        Some(serde_json::Value::String(s)) => s,
        _ => return false,
    };

    // Parse the error string to extract the actual rollup height
    // Pattern: "Block state assertion failed at rollup height <actual_height>."
    let actual_height = match extract_rollup_height_from_error(&error_str) {
        Some(h) => h,
        None => return false,
    };

    // If the actual height is greater than expected, this is a "too late" error
    actual_height > expected_height
}

/// Extract the rollup height from the error message.
/// Pattern: "Block state assertion failed at rollup height <height>."
fn extract_rollup_height_from_error(error_str: &str) -> Option<u64> {
    // Find "Block state assertion failed at rollup height "
    let prefix = "Block state assertion failed at rollup height ";
    let start_idx = error_str.find(prefix)?;
    let height_start = start_idx + prefix.len();

    // Find the next non-digit character (should be '.')
    let remaining = &error_str[height_start..];
    let height_str = remaining.split(|c: char| !c.is_ascii_digit()).next()?;

    height_str.parse::<u64>().ok()
}

pub async fn state_validation_worker(
    client: sov_api_spec::Client,
    rollup_stop_height: u64,
    rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Subscribe to slots
    let mut slot_stream = client
        .subscribe_slots_with_children(IncludeChildren::new(false))
        .await?;

    tracing::info!("State validation worker started");

    let mut last_visible_slot = query_visible_slot(&client).await?;
    while !*rx.borrow() {
        let Some(latest_slot) = drain_slot_stream(&mut slot_stream).await? else {
            break; // Stream closed
        };

        if latest_slot.number >= rollup_stop_height {
            // We're at, or very close to, the rollup shutting down. We can stop the state
            // assertions now, and avoid having to handle failures due to the rollup no longer
            // responding.
            tracing::info!("State validation worker reached rollup_stop_height {rollup_stop_height}. Shutting down worker.");
            break;
        }

        // There's a race condition between slot notification and API state update, so poll until
        // the new visible slot is actually visible in the API
        let visible_slot =
            poll_for_visible_slot_update(&client, latest_slot.number, last_visible_slot).await?;

        // Get the other two kernel values
        let (state_root_result, rollup_height_result) =
            tokio::join!(async { query_state_root(&client).await }, async {
                query_rollup_height(&client).await
            });
        let state_root = state_root_result?;
        let rollup_height = rollup_height_result?;

        // Now we can submit the assertion transaction
        tracing::debug!(
            "Visible_slot: {visible_slot}, rollup_height: {rollup_height}, state_root: {}",
            hex::encode(&state_root.root_hashes)
        );
        send_assert_block_tx(&client, visible_slot, rollup_height, state_root.root_hashes).await?;

        last_visible_slot = visible_slot;
    }

    tracing::info!("State validation worker shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_rollup_height_from_error() {
        let error_msg = "TransactionReceipt { tx_hash: 0x7e55c8eb41c10c756d1a226bf104297e1e328317102485b0bc6b1498bc4283cc, body_to_save: \"<removed>\", events: [], receipt: Reverted(RevertedTxContents { gas_used: GasUnit[21287, 21287], reason: ModuleError(Block state assertion failed at rollup height 14. List of mismatches: [\"Rollup height mismatch. Transaction expected 13, but actual state is 14 (sanity check: saved state from begin block hook is 14)\"]) }) }";

        let height = extract_rollup_height_from_error(error_msg);
        assert_eq!(height, Some(14));
    }

    #[test]
    fn test_extract_rollup_height_no_match() {
        let error_msg = "Some other error message";
        let height = extract_rollup_height_from_error(error_msg);
        assert_eq!(height, None);
    }
}
