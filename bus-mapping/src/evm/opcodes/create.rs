use crate::circuit_input_builder::{CircuitInputStateRef, ExecStep};
use crate::evm::Opcode;
use crate::operation::{AccountField, AccountOp, TxAccessListAccountOp, RW};
use crate::Error;
use eth_types::GethExecStep;
use keccak256::EMPTY_HASH;

#[derive(Debug, Copy, Clone)]
pub struct DummyCreate;

impl Opcode for DummyCreate {
    fn gen_associated_ops(
        state: &mut CircuitInputStateRef,
        geth_steps: &[GethExecStep],
    ) -> Result<Vec<ExecStep>, Error> {
        // TODO: replace dummy create here
        dummy_gen_create_ops(state, geth_steps)
    }
}

fn dummy_gen_create_ops(
    state: &mut CircuitInputStateRef,
    geth_steps: &[GethExecStep],
) -> Result<Vec<ExecStep>, Error> {
    let geth_step = &geth_steps[0];

    let offset = geth_step.stack.nth_last(1)?.as_usize();
    let length = geth_step.stack.nth_last(2)?.as_usize();

    if length != 0 {
        state
            .call_ctx_mut()?
            .memory
            .extend_at_least(offset + length);
    }

    let mut exec_step = state.new_step(geth_step)?;

    let tx_id = state.tx_ctx.id();
    let call = state.parse_call(geth_step)?;

    // Increase caller's nonce
    let nonce_prev = state.sdb.get_nonce(&call.caller_address);
    state.push_op_reversible(
        &mut exec_step,
        RW::WRITE,
        AccountOp {
            address: call.caller_address,
            field: AccountField::Nonce,
            value: (nonce_prev + 1).into(),
            value_prev: nonce_prev.into(),
        },
    )?;

    // Add callee into access list
    let is_warm = state.sdb.check_account_in_access_list(&call.address);
    state.push_op_reversible(
        &mut exec_step,
        RW::WRITE,
        TxAccessListAccountOp {
            tx_id,
            address: call.address,
            is_warm: true,
            is_warm_prev: is_warm,
        },
    )?;

    state.push_call(call.clone());

    // Increase callee's nonce
    let nonce_prev = state.sdb.get_nonce(&call.address);
    debug_assert!(nonce_prev == 0);
    state.push_op_reversible(
        &mut exec_step,
        RW::WRITE,
        AccountOp {
            address: call.address,
            field: AccountField::Nonce,
            value: 1.into(),
            value_prev: 0.into(),
        },
    )?;

    state.transfer(
        &mut exec_step,
        call.caller_address,
        call.address,
        call.value,
    )?;

    if call.code_hash.to_fixed_bytes() == *EMPTY_HASH {
        // 1. Create with empty initcode.
        state.handle_return(geth_step)?;
        Ok(vec![exec_step])
    } else {
        // 2. Create with non-empty initcode.
        Ok(vec![exec_step])
    }
}
