//! Owned CPU-dispatch boundaries and explicit, nonblocking local delivery.
//!
//! Endpoint and MCP routing, full request accounting, and scheduling belong to
//! the configured dispatcher. The bridge never obtains a reactor or executor.
//! Only owned requests and completions cross that seam; the transport remains
//! inside its reactor-local binding.

use super::*;

/// Transfers one complete protected message to the configured dispatcher.
///
/// Admission requires nonzero CPU and timeout allowances and enough retained
/// bytes for the input. The dispatcher must additionally account for decoded
/// storage, scratch, and output within the unchanged request budget. Protection
/// establishes transport completeness, so JSON and schema validation still
/// belong to the dispatcher. Dispatcher errors propagate without retrying.
pub fn submit_input(
    dispatcher: &mut dyn OwnedDispatcher,
    connection: ConnectionId,
    input: ProtectedInput,
    budget: ResourceBudget,
) -> Result<()> {
    ensure_request_budget(input.payload().len(), budget)?;
    dispatcher.submit(BridgeRequest { connection, input, budget })
}

/// Polls once and transfers at most one validated output to its connection.
///
/// Returns `false` only when the poll has no completion. Poll, completion, and
/// transport failures propagate unchanged. A consumed completion is never
/// retried, including when the transport reports backpressure. The host must
/// provide retry storage if needed. `try_send` must enqueue the entire immutable
/// frame atomically or fail without publishing a partial frame.
///
/// The transport must reject completions for closed connections. Connection IDs
/// must not be reused while old work can complete; the bridge has no connection
/// registry and cannot distinguish different lifetimes sharing the same ID.
pub fn forward_one<T: LocalTransport>(
    binding: &mut ReactorBinding<T>,
    dispatcher: &mut dyn OwnedDispatcher,
) -> Result<bool> {
    let completion = dispatcher.poll_completion()?.map(|completion| {
        (completion.connection, completion.output)
    });
    deliver_completion(completion, |connection, output| {
        binding.transport_mut().try_send(connection, output)
    })
}

/// Validates and requests one nonblocking transport close.
///
/// The reason is already valid UTF-8 through its `str` type and may contain at
/// most 123 bytes, leaving two bytes for the status code. It need not be JSON.
/// Invalid arguments never reach the transport. The transport owns connection
/// lifecycle bookkeeping and reports any close failure unchanged.
pub fn close_connection<T: LocalTransport>(
    binding: &mut ReactorBinding<T>,
    connection: ConnectionId,
    code: u16,
    reason: &str,
) -> Result<()> {
    if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
        return Err(transport_error(ErrorCode::FramingError, "invalid WebSocket close code"));
    }
    if reason.len() > MAX_CONTROL_BYTES - 2 {
        return Err(transport_error(
            ErrorCode::PayloadTooLarge,
            "WebSocket close reason exceeds 123 bytes",
        ));
    }
    binding.transport_mut().close(connection, code, reason)
}

fn ensure_request_budget(payload_len: usize, budget: ResourceBudget) -> Result<()> {
    if budget.cpu_units == 0
        || budget.timeout_ms == 0
        || u32::try_from(payload_len).map_or(true, |len| len > budget.retained_bytes)
    {
        return Err(transport_error(
            ErrorCode::BudgetExhausted,
            "WebSocket request exceeds CPU, timeout, or retained byte budget",
        ));
    }
    Ok(())
}

// Ownership delivery does not inspect, clone, or manufacture its output. The
// public entry point specializes this helper exclusively to ValidatedOutput.
fn deliver_completion<O>(
    completion: Option<(ConnectionId, Result<O>)>,
    send: impl FnOnce(ConnectionId, O) -> Result<()>,
) -> Result<bool> {
    let Some((connection, output)) = completion else {
        return Ok(false);
    };
    send(connection, output?)?;
    Ok(true)
}
