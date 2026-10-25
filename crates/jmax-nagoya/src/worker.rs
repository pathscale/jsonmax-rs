//! Canonical compiled-schema worker for owned CPU submissions.

use super::*;
use jmax_abi::{number::NumericPolicy, schema::Selection};
use jmax_schema_vm::{ValidatedProgram, VmLimits};

/// Immutable compiled schema and extraction plan shared by queued jobs. The
/// schema is compiled once by the host, then retained through Arc handles;
/// no schema compilation or payload staging occurs in the worker.
pub struct SchemaValidator {
    program: Arc<ValidatedProgram>,
    selection: Selection,
    numeric: NumericPolicy,
    max_steps: u64,
}

impl SchemaValidator {
    pub fn new(
        program: Arc<ValidatedProgram>,
        selection: Selection,
        numeric: NumericPolicy,
        max_steps: u64,
    ) -> Result<Self> {
        if max_steps == 0 {
            return Err(runtime_error(
                ErrorCode::ConstraintViolation,
                "VM step budget must be nonzero",
            ));
        }
        jmax_schema_vm::check_selection(&program, &selection)?;
        Ok(Self {
            program,
            selection,
            numeric,
            max_steps,
        })
    }

    pub fn program(&self) -> &Arc<ValidatedProgram> {
        &self.program
    }
}

impl ParseValidator for SchemaValidator {
    fn validate(
        &self,
        payload: Payload,
        limits: Limits,
        scratch: &mut Scratch,
        scratch_budget_bytes: usize,
        retained_budget_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<ValidatedSlots> {
        self.validate_output(
            payload,
            limits,
            scratch,
            scratch_budget_bytes,
            retained_budget_bytes,
            cancellation,
        )
        .map(ValidationOutput::into_slots)
    }

    fn validate_output(
        &self,
        payload: Payload,
        limits: Limits,
        scratch: &mut Scratch,
        scratch_budget_bytes: usize,
        retained_budget_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<ValidationOutput> {
        cancellation.check()?;
        // Do not retain unused generic scratch alongside the VM's own scratch.
        *scratch = Scratch::default();
        // The direct VM owns its bounded internal scratch for this invocation;
        // the generic syntax Scratch stays empty. VM steps bound the synchronous
        // turn; cancellation is checked before and after that turn.
        let vm_limits = VmLimits {
            syntax: limits,
            max_steps: self.max_steps,
            max_scratch_bytes: u32::try_from(scratch_budget_bytes).unwrap_or(u32::MAX),
        };
        let original = payload.clone();
        let output = jmax_schema_vm::extract_payload_direct(
            &self.program,
            payload,
            &self.selection,
            vm_limits,
            self.numeric,
        )?;
        cancellation.check()?;
        // SAFETY: the direct strict VM just validated these exact immutable
        // owners and produced this proof; no public mutation intervenes.
        let capability = unsafe {
            ValidationCapability::issue_for_payload(
                &output.payload,
                output.proof.decoded_len(),
                self.program.image().header.fingerprint,
                jmax_schema_vm::direct::policy_fingerprint(vm_limits, self.numeric),
            )
        };
        let output = ValidationOutput {
            slots: output,
            capability: Some(capability),
        };
        submit::check_output(&original, &output, retained_budget_bytes)?;
        Ok(output)
    }
}
