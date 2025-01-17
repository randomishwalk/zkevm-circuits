use super::Opcode;
use crate::circuit_input_builder::{CircuitInputStateRef, ExecStep};
use crate::circuit_input_builder::{CopyDataType, CopyEvent, CopyStep, NumberOrHash};
use crate::operation::{CallContextField, MemoryOp, RW};
use crate::Error;
use eth_types::GethExecStep;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Calldatacopy;

impl Opcode for Calldatacopy {
    fn gen_associated_ops(
        state: &mut CircuitInputStateRef,
        geth_steps: &[GethExecStep],
    ) -> Result<Vec<ExecStep>, Error> {
        let geth_step = &geth_steps[0];
        let exec_steps = vec![gen_calldatacopy_step(state, geth_step)?];

        // reconstruction
        let memory_offset = geth_step.stack.nth_last(0)?.as_u64();
        let data_offset = geth_step.stack.nth_last(1)?.as_u64();
        let length = geth_step.stack.nth_last(2)?.as_usize();
        let call_ctx = state.call_ctx_mut()?;
        let memory = &mut call_ctx.memory;
        if length != 0 {
            let minimal_length = memory_offset as usize + length;
            memory.extend_at_least(minimal_length);

            let mem_starts = memory_offset as usize;
            let mem_ends = mem_starts + length as usize;
            let data_starts = data_offset as usize;
            let data_ends = data_starts + length as usize;
            let call_data = &call_ctx.call_data;
            if data_ends <= call_data.len() {
                memory.0[mem_starts..mem_ends].copy_from_slice(&call_data[data_starts..data_ends]);
            } else if let Some(actual_length) = call_data.len().checked_sub(data_starts) {
                let mem_code_ends = mem_starts + actual_length;
                memory.0[mem_starts..mem_code_ends].copy_from_slice(&call_data[data_starts..]);
                // since we already resize the memory, no need to copy 0s for
                // out of bound bytes
            }
        }

        let copy_event = gen_copy_event(state, geth_step)?;
        state.push_copy(copy_event);
        Ok(exec_steps)
    }
}

fn gen_calldatacopy_step(
    state: &mut CircuitInputStateRef,
    geth_step: &GethExecStep,
) -> Result<ExecStep, Error> {
    let mut exec_step = state.new_step(geth_step)?;
    let memory_offset = geth_step.stack.nth_last(0)?;
    let data_offset = geth_step.stack.nth_last(1)?;
    let length = geth_step.stack.nth_last(2)?;

    state.stack_read(
        &mut exec_step,
        geth_step.stack.nth_last_filled(0),
        memory_offset,
    )?;
    state.stack_read(
        &mut exec_step,
        geth_step.stack.nth_last_filled(1),
        data_offset,
    )?;
    state.stack_read(&mut exec_step, geth_step.stack.nth_last_filled(2), length)?;

    if state.call()?.is_root {
        state.call_context_read(
            &mut exec_step,
            state.call()?.call_id,
            CallContextField::TxId,
            state.tx_ctx.id().into(),
        );
        state.call_context_read(
            &mut exec_step,
            state.call()?.call_id,
            CallContextField::CallDataLength,
            state.call()?.call_data_length.into(),
        );
    } else {
        state.call_context_read(
            &mut exec_step,
            state.call()?.call_id,
            CallContextField::CallerId,
            state.call()?.caller_id.into(),
        );
        state.call_context_read(
            &mut exec_step,
            state.call()?.call_id,
            CallContextField::CallDataLength,
            state.call()?.call_data_length.into(),
        );
        state.call_context_read(
            &mut exec_step,
            state.call()?.call_id,
            CallContextField::CallDataOffset,
            state.call()?.call_data_offset.into(),
        );
    };

    Ok(exec_step)
}

fn gen_copy_steps(
    state: &mut CircuitInputStateRef,
    exec_step: &mut ExecStep,
    src_addr: u64,
    dst_addr: u64,
    src_addr_end: u64,
    bytes_left: u64,
    is_root: bool,
) -> Result<Vec<CopyStep>, Error> {
    let mut copy_steps = Vec::with_capacity(2 * bytes_left as usize);
    for idx in 0..bytes_left {
        let addr = src_addr + idx;
        let rwc = state.block_ctx.rwc;
        let (value, is_pad) = if addr < src_addr_end {
            let byte =
                state.call_ctx()?.call_data[(addr - state.call()?.call_data_offset) as usize];
            if !is_root {
                state.push_op(
                    exec_step,
                    RW::READ,
                    MemoryOp::new(state.call()?.caller_id, addr.into(), byte),
                );
                (byte, false)
            } else {
                (byte, false)
            }
        } else {
            (0, true)
        };
        let tag = if is_root {
            CopyDataType::TxCalldata
        } else {
            CopyDataType::Memory
        };
        // Read
        copy_steps.push(CopyStep {
            addr,
            tag,
            rw: RW::READ,
            value,
            is_code: None,
            is_pad,
            rwc,
            rwc_inc_left: 0,
        });
        // Write
        copy_steps.push(CopyStep {
            addr: dst_addr + idx,
            tag: CopyDataType::Memory,
            rw: RW::WRITE,
            value,
            is_code: None,
            is_pad: false,
            rwc: state.block_ctx.rwc,
            rwc_inc_left: 0,
        });
        state.memory_write(exec_step, (dst_addr + idx).into(), value)?;
    }

    for cs in copy_steps.iter_mut() {
        cs.rwc_inc_left = state.block_ctx.rwc.0 as u64 - cs.rwc.0 as u64;
    }

    Ok(copy_steps)
}

fn gen_copy_event(
    state: &mut CircuitInputStateRef,
    geth_step: &GethExecStep,
) -> Result<CopyEvent, Error> {
    let memory_offset = geth_step.stack.nth_last(0)?.as_u64();
    let data_offset = geth_step.stack.nth_last(1)?.as_u64();
    let length = geth_step.stack.nth_last(2)?.as_u64();

    let call_data_offset = state.call()?.call_data_offset;
    let call_data_length = state.call()?.call_data_length;
    let (src_addr, src_addr_end) = (
        call_data_offset + data_offset,
        call_data_offset + call_data_length,
    );

    let mut exec_step = state.new_step(geth_step)?;
    let copy_steps = gen_copy_steps(
        state,
        &mut exec_step,
        src_addr,
        memory_offset,
        src_addr_end,
        length,
        state.call()?.is_root,
    )?;

    let (src_type, src_id) = if state.call()?.is_root {
        (CopyDataType::TxCalldata, state.tx_ctx.id())
    } else {
        (CopyDataType::Memory, state.call()?.caller_id)
    };

    Ok(CopyEvent {
        src_type,
        src_id: NumberOrHash::Number(src_id),
        src_addr,
        src_addr_end,
        dst_type: CopyDataType::Memory,
        dst_id: NumberOrHash::Number(state.call()?.call_id),
        dst_addr: memory_offset,
        log_id: None,
        length,
        steps: copy_steps,
        tx_id: state.tx_ctx.id(),
        call_id: state.call()?.call_id,
        pc: exec_step.pc,
    })
}

#[cfg(test)]
mod calldatacopy_tests {
    use crate::{
        circuit_input_builder::{CopyDataType, CopyStep, ExecState, NumberOrHash},
        mock::BlockData,
        operation::{CallContextField, CallContextOp, MemoryOp, RWCounter, StackOp, RW},
    };
    use eth_types::{
        bytecode,
        evm_types::{OpcodeId, StackAddress},
        geth_types::GethData,
        ToWord, Word,
    };

    use mock::test_ctx::{helpers::*, TestContext};
    use pretty_assertions::assert_eq;

    #[test]
    fn calldatacopy_opcode_internal() {
        let (addr_a, addr_b) = (mock::MOCK_ACCOUNTS[0], mock::MOCK_ACCOUNTS[1]);

        // code B gets called by code A, so the call is an internal call.
        let dst_offset = 0x00usize;
        let offset = 0x00usize;
        let copy_size = 0x10usize;
        let code_b = bytecode! {
            PUSH32(copy_size)  // size
            PUSH32(offset)     // offset
            PUSH32(dst_offset) // dst_offset
            CALLDATACOPY
            STOP
        };

        // code A calls code B.
        let pushdata = hex::decode("1234567890abcdef").unwrap();
        let memory_a = std::iter::repeat(0)
            .take(24)
            .chain(pushdata.clone())
            .collect::<Vec<u8>>();
        let call_data_length = 0x20usize;
        let call_data_offset = 0x10usize;
        let code_a = bytecode! {
            // populate memory in A's context.
            PUSH8(Word::from_big_endian(&pushdata))
            PUSH1(0x00) // offset
            MSTORE
            // call addr_b.
            PUSH1(0x00) // retLength
            PUSH1(0x00) // retOffset
            PUSH1(call_data_length) // argsLength
            PUSH1(call_data_offset) // argsOffset
            PUSH1(0x00) // value
            PUSH32(addr_b.to_word()) // addr
            PUSH32(0x1_0000) // gas
            CALL
            STOP
        };

        // Get the execution steps from the external tracer
        let block: GethData = TestContext::<3, 1>::new(
            None,
            |accs| {
                accs[0].address(addr_b).code(code_b);
                accs[1].address(addr_a).code(code_a);
                accs[2]
                    .address(mock::MOCK_ACCOUNTS[2])
                    .balance(Word::from(1u64 << 30));
            },
            |mut txs, accs| {
                txs[0].to(accs[1].address).from(accs[2].address);
            },
            |block, _tx| block,
        )
        .unwrap()
        .into();

        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();

        let step = builder.block.txs()[0]
            .steps()
            .iter()
            .find(|step| step.exec_state == ExecState::Op(OpcodeId::CALLDATACOPY))
            .unwrap();

        let caller_id = builder.block.txs()[0].calls()[step.call_index].caller_id;
        let expected_call_id = builder.block.txs()[0].calls()[step.call_index].call_id;

        // 3 stack reads + 3 call context reads.
        assert_eq!(step.bus_mapping_instance.len(), 6);

        // 3 stack reads.
        assert_eq!(
            [0, 1, 2]
                .map(|idx| &builder.block.container.stack[step.bus_mapping_instance[idx].as_usize()])
                .map(|operation| (operation.rw(), operation.op())),
            [
                (
                    RW::READ,
                    &StackOp::new(expected_call_id, StackAddress::from(1021), Word::from(dst_offset))
                ),
                (
                    RW::READ,
                    &StackOp::new(expected_call_id, StackAddress::from(1022), Word::from(offset))
                ),
                (
                    RW::READ,
                    &StackOp::new(expected_call_id, StackAddress::from(1023), Word::from(copy_size))
                ),
            ]
        );

        // 3 call context reads.
        assert_eq!(
            [3, 4, 5]
                .map(|idx| &builder.block.container.call_context
                    [step.bus_mapping_instance[idx].as_usize()])
                .map(|operation| (operation.rw(), operation.op())),
            [
                (
                    RW::READ,
                    &CallContextOp {
                        call_id: expected_call_id,
                        field: CallContextField::CallerId,
                        value: Word::from(1),
                    }
                ),
                (
                    RW::READ,
                    &CallContextOp {
                        call_id: expected_call_id,
                        field: CallContextField::CallDataLength,
                        value: Word::from(call_data_length),
                    },
                ),
                (
                    RW::READ,
                    &CallContextOp {
                        call_id: expected_call_id,
                        field: CallContextField::CallDataOffset,
                        value: Word::from(call_data_offset),
                    },
                ),
            ]
        );

        // Memory reads/writes.
        //
        // 1. First `call_data_length` memory ops are RW::WRITE and come from the `CALL`
        // opcode. We skip checking those.
        //
        // 2. Following that, we should have tuples of (RW::READ and RW::WRITE) where
        // the caller memory is read and the current call's memory is written to.
        assert_eq!(
            builder.block.container.memory.len(),
            call_data_length + 2 * copy_size
        );
        assert_eq!(
            (call_data_length..(call_data_length + (2 * copy_size)))
                .map(|idx| &builder.block.container.memory[idx])
                .map(|op| (op.rw(), op.op().clone()))
                .collect::<Vec<(RW, MemoryOp)>>(),
            {
                let mut memory_ops = Vec::with_capacity(2 * copy_size);
                (0..copy_size).for_each(|idx| {
                    let value = if offset + call_data_offset + idx < memory_a.len() {
                        memory_a[offset + call_data_offset + idx]
                    } else {
                        0
                    };
                    memory_ops.push((
                        RW::READ,
                        MemoryOp::new(caller_id, (call_data_offset + offset + idx).into(), value),
                    ));
                    memory_ops.push((
                        RW::WRITE,
                        MemoryOp::new(expected_call_id, (dst_offset + idx).into(), value),
                    ));
                });
                memory_ops
            },
        );

        let copy_events = builder.block.copy_events.clone();
        assert_eq!(copy_events.len(), 1);
        assert_eq!(copy_events[0].steps.len(), 2 * copy_size);
        assert_eq!(copy_events[0].src_id, NumberOrHash::Number(caller_id));
        assert_eq!(
            copy_events[0].dst_id,
            NumberOrHash::Number(expected_call_id)
        );
        assert_eq!(copy_events[0].length, copy_size as u64);
        assert!(copy_events[0].log_id.is_none());
        assert_eq!(copy_events[0].src_addr as usize, offset + call_data_offset);
        assert_eq!(
            copy_events[0].src_addr_end as usize,
            offset + call_data_offset + call_data_length
        );
        assert_eq!(copy_events[0].dst_addr as usize, dst_offset);

        let mut rwc = RWCounter(step.rwc.0 + 6);
        let mut rwc_inc = copy_events[0].steps.first().unwrap().rwc_inc_left;
        for (idx, copy_rw_pair) in copy_events[0].steps.chunks(2).enumerate() {
            assert_eq!(copy_rw_pair.len(), 2);
            let (value, is_pad) = memory_a
                .get(offset + call_data_offset + idx)
                .cloned()
                .map_or((0, true), |v| (v, false));
            // Read
            let read_step = copy_rw_pair[0].clone();
            assert_eq!(
                read_step,
                CopyStep {
                    addr: (offset + call_data_offset + idx) as u64,
                    tag: CopyDataType::Memory,
                    rw: RW::READ,
                    is_code: None,
                    value,
                    is_pad,
                    rwc: if !is_pad { rwc.inc_pre() } else { rwc },
                    rwc_inc_left: rwc_inc,
                }
            );
            if !is_pad {
                rwc_inc -= 1;
            }
            // Write
            let write_step = copy_rw_pair[1].clone();
            assert_eq!(
                write_step,
                CopyStep {
                    addr: (dst_offset + idx) as u64,
                    tag: CopyDataType::Memory,
                    rw: RW::WRITE,
                    is_code: None,
                    value,
                    is_pad: false,
                    rwc: rwc.inc_pre(),
                    rwc_inc_left: rwc_inc,
                }
            );
            rwc_inc -= 1;
        }
    }

    #[test]
    fn calldatacopy_opcode_internal_overflow() {
        let (addr_a, addr_b) = (mock::MOCK_ACCOUNTS[0], mock::MOCK_ACCOUNTS[1]);

        // code B gets called by code A, so the call is an internal call.
        let dst_offset = 0x00usize;
        let offset = 0x00usize;
        let copy_size = 0x50usize;
        let code_b = bytecode! {
            PUSH32(copy_size)  // size
            PUSH32(offset)     // offset
            PUSH32(dst_offset) // dst_offset
            CALLDATACOPY
            STOP
        };

        // code A calls code B.
        let pushdata = hex::decode("1234567890abcdef").unwrap();
        let _memory_a = std::iter::repeat(0)
            .take(24)
            .chain(pushdata.clone())
            .collect::<Vec<u8>>();
        let call_data_length = 0x20usize;
        let call_data_offset = 0x10usize;
        let code_a = bytecode! {
            // populate memory in A's context.
            PUSH8(Word::from_big_endian(&pushdata))
            PUSH1(0x00) // offset
            MSTORE
            // call addr_b.
            PUSH1(0x00) // retLength
            PUSH1(0x00) // retOffset
            PUSH1(call_data_length) // argsLength
            PUSH1(call_data_offset) // argsOffset
            PUSH1(0x00) // value
            PUSH32(addr_b.to_word()) // addr
            PUSH32(0x1_0000) // gas
            CALL
            STOP
        };

        // Get the execution steps from the external tracer
        let block: GethData = TestContext::<3, 1>::new(
            None,
            |accs| {
                accs[0].address(addr_b).code(code_b);
                accs[1].address(addr_a).code(code_a);
                accs[2]
                    .address(mock::MOCK_ACCOUNTS[2])
                    .balance(Word::from(1u64 << 30));
            },
            |mut txs, accs| {
                txs[0].to(accs[1].address).from(accs[2].address);
            },
            |block, _tx| block,
        )
        .unwrap()
        .into();

        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();
    }

    #[test]
    fn calldatacopy_opcode_root() {
        let size = 0x40;
        let offset = 0x00;
        let dst_offset = 0x00;
        let calldata = vec![1, 3, 5, 7, 9, 2, 4, 6, 8];
        let calldata_len = calldata.len();
        let code = bytecode! {
            PUSH32(size)
            PUSH32(offset)
            PUSH32(dst_offset)
            CALLDATACOPY
            STOP
        };

        // Get the execution steps from the external tracer
        let block: GethData = TestContext::<2, 1>::new(
            None,
            account_0_code_account_1_no_code(code),
            |mut txs, accs| {
                txs[0]
                    .to(accs[0].address)
                    .from(accs[1].address)
                    .input(calldata.clone().into());
            },
            |block, _tx| block,
        )
        .unwrap()
        .into();

        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();

        let step = builder.block.txs()[0]
            .steps()
            .iter()
            .find(|step| step.exec_state == ExecState::Op(OpcodeId::CALLDATACOPY))
            .unwrap();

        let expected_call_id = builder.block.txs()[0].calls()[step.call_index].call_id;
        assert_eq!(step.bus_mapping_instance.len(), 5);

        assert_eq!(
            [0, 1, 2]
                .map(|idx| &builder.block.container.stack[step.bus_mapping_instance[idx].as_usize()])
                .map(|operation| (operation.rw(), operation.op())),
            [
                (
                    RW::READ,
                    &StackOp::new(1, StackAddress::from(1021), dst_offset.into())
                ),
                (
                    RW::READ,
                    &StackOp::new(1, StackAddress::from(1022), offset.into())
                ),
                (
                    RW::READ,
                    &StackOp::new(1, StackAddress::from(1023), size.into())
                ),
            ]
        );

        assert_eq!(
            [3, 4]
                .map(|idx| &builder.block.container.call_context
                    [step.bus_mapping_instance[idx].as_usize()])
                .map(|operation| (operation.rw(), operation.op())),
            [
                (
                    RW::READ,
                    &CallContextOp {
                        call_id: builder.block.txs()[0].calls()[0].call_id,
                        field: CallContextField::TxId,
                        value: Word::from(1),
                    }
                ),
                (
                    RW::READ,
                    &CallContextOp {
                        call_id: builder.block.txs()[0].calls()[0].call_id,
                        field: CallContextField::CallDataLength,
                        value: calldata_len.into(),
                    },
                ),
            ]
        );

        // Memory reads/writes.
        //
        // 1. Since its a root call, we should only have memory RW::WRITE where the
        // current call's memory is written to.
        assert_eq!(builder.block.container.memory.len(), size);
        assert_eq!(
            (0..size)
                .map(|idx| &builder.block.container.memory[idx])
                .map(|op| (op.rw(), op.op().clone()))
                .collect::<Vec<(RW, MemoryOp)>>(),
            {
                let mut memory_ops = Vec::with_capacity(size);
                (0..size).for_each(|idx| {
                    let value = if offset + idx < calldata_len {
                        calldata[offset + idx]
                    } else {
                        0
                    };
                    memory_ops.push((
                        RW::WRITE,
                        MemoryOp::new(expected_call_id, (dst_offset + idx).into(), value),
                    ));
                });
                memory_ops
            },
        );

        let copy_events = builder.block.copy_events.clone();

        // single copy event with `size` reads and `size` writes.
        assert_eq!(copy_events.len(), 1);
        assert_eq!(copy_events[0].steps.len(), 2 * size);

        let mut rwc = RWCounter(step.rwc.0 + 5);
        for (idx, copy_rw_pair) in copy_events[0].steps.chunks(2).enumerate() {
            assert_eq!(copy_rw_pair.len(), 2);
            let (value, is_pad) = calldata
                .get(offset as usize + idx)
                .cloned()
                .map_or((0, true), |v| (v, false));
            // read
            let read_step = copy_rw_pair[0].clone();
            assert_eq!(
                read_step,
                CopyStep {
                    addr: (offset + idx) as u64,
                    tag: CopyDataType::TxCalldata,
                    rw: RW::READ,
                    value,
                    is_code: None,
                    is_pad,
                    rwc,
                    rwc_inc_left: (size - idx) as u64,
                }
            );
            // write
            let write_step = copy_rw_pair[1].clone();
            assert_eq!(
                write_step,
                CopyStep {
                    addr: (dst_offset + idx) as u64,
                    tag: CopyDataType::Memory,
                    rw: RW::WRITE,
                    value,
                    is_code: None,
                    is_pad: false,
                    rwc: rwc.inc_pre(),
                    rwc_inc_left: (size - idx) as u64,
                }
            );
        }
    }
}
