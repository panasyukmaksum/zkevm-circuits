//! The EVM circuit implementation.

#![allow(missing_docs)]
use halo2_proofs::{circuit::Layouter, plonk::*};

mod execution;
pub mod param;
mod step;
pub(crate) mod util;

pub mod table;
pub mod witness;

use crate::table::{BytecodeTable, TxTable};
use eth_types::Field;
use execution::ExecutionConfig;
use itertools::Itertools;
use table::{FixedTableTag, LookupTable};
use witness::Block;

/// EvmCircuit implements verification of execution trace of a block.
#[derive(Clone, Debug)]
pub struct EvmCircuit<F> {
    fixed_table: [Column<Fixed>; 4],
    byte_table: [Column<Fixed>; 1],
    execution: Box<ExecutionConfig<F>>,
}

impl<F: Field> EvmCircuit<F> {
    /// Configure EvmCircuit
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        power_of_randomness: [Expression<F>; 31],
        tx_table: &dyn LookupTable<F>,
        rw_table: &dyn LookupTable<F>,
        bytecode_table: &dyn LookupTable<F>,
        block_table: &dyn LookupTable<F>,
    ) -> Self {
        let fixed_table = [(); 4].map(|_| meta.fixed_column());
        let byte_table = [(); 1].map(|_| meta.fixed_column());

        let execution = Box::new(ExecutionConfig::configure(
            meta,
            power_of_randomness,
            &fixed_table,
            &byte_table,
            tx_table,
            rw_table,
            bytecode_table,
            block_table,
        ));

        Self {
            fixed_table,
            byte_table,
            execution,
        }
    }

    /// Load fixed table
    pub fn load_fixed_table(
        &self,
        layouter: &mut impl Layouter<F>,
        fixed_table_tags: Vec<FixedTableTag>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "fixed table",
            |mut region| {
                for (offset, row) in std::iter::once([F::zero(); 4])
                    .chain(fixed_table_tags.iter().flat_map(|tag| tag.build()))
                    .enumerate()
                {
                    for (column, value) in self.fixed_table.iter().zip_eq(row) {
                        region.assign_fixed(|| "", *column, offset, || Ok(value))?;
                    }
                }

                Ok(())
            },
        )
    }

    /// Load byte table
    pub fn load_byte_table(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        layouter.assign_region(
            || "byte table",
            |mut region| {
                for offset in 0..256 {
                    region.assign_fixed(
                        || "",
                        self.byte_table[0],
                        offset,
                        || Ok(F::from(offset as u64)),
                    )?;
                }

                Ok(())
            },
        )
    }

    /// Assign block
    pub fn assign_block(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
    ) -> Result<(), Error> {
        self.execution.assign_block(layouter, block, false)
    }

    /// Assign exact steps in block without padding for unit test purpose
    #[cfg(any(feature = "test", test))]
    pub fn assign_block_exact(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
    ) -> Result<(), Error> {
        self.execution.assign_block(layouter, block, true)
    }

    /// Calculate which rows are "actually" used in the circuit
    pub fn get_active_rows(&self, block: &Block<F>) -> (Vec<usize>, Vec<usize>) {
        let max_offset = self.get_num_rows_required(block);
        // some gates are enabled on all rows
        let gates_row_ids = (0..max_offset).collect();
        // lookups are enabled at "q_step" rows and byte lookup rows
        let lookup_row_ids = (0..max_offset).collect();
        (gates_row_ids, lookup_row_ids)
    }

    pub fn get_num_rows_required(&self, block: &Block<F>) -> usize {
        // Start at 1 so we can be sure there is an unused `next` row available
        let mut num_rows = 1;
        for transaction in &block.txs {
            for step in &transaction.steps {
                num_rows += self.execution.get_step_height(step.execution_state);
            }
        }
        num_rows
    }
}

#[cfg(any(feature = "test", test))]
pub mod test {
    use super::*;
    use crate::{
        evm_circuit::{table::FixedTableTag, witness::Block, EvmCircuit},
        table::{load_block, load_bytecodes, load_rws, load_txs, BlockTable, RwTable},
        util::power_of_randomness_from_instance,
    };
    use eth_types::{Field, Word};
    use halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner},
        dev::{MockProver, VerifyFailure},
        plonk::{Circuit, ConstraintSystem, Error},
    };
    use rand::{
        distributions::uniform::{SampleRange, SampleUniform},
        random, thread_rng, Rng,
    };
    use strum::IntoEnumIterator;

    pub(crate) fn rand_range<T, R>(range: R) -> T
    where
        T: SampleUniform,
        R: SampleRange<T>,
    {
        thread_rng().gen_range(range)
    }

    pub(crate) fn rand_bytes(n: usize) -> Vec<u8> {
        (0..n).map(|_| random()).collect()
    }

    pub(crate) fn rand_bytes_array<const N: usize>() -> [u8; N] {
        [(); N].map(|_| random())
    }

    pub(crate) fn rand_word() -> Word {
        Word::from_big_endian(&rand_bytes_array::<32>())
    }

    #[derive(Clone)]
    pub struct TestCircuitConfig<F> {
        tx_table: TxTable,
        rw_table: RwTable,
        bytecode_table: BytecodeTable,
        block_table: BlockTable,
        evm_circuit: EvmCircuit<F>,
    }

    #[derive(Default)]
    pub struct TestCircuit<F> {
        block: Block<F>,
        fixed_table_tags: Vec<FixedTableTag>,
    }

    impl<F> TestCircuit<F> {
        pub fn new(block: Block<F>, fixed_table_tags: Vec<FixedTableTag>) -> Self {
            Self {
                block,
                fixed_table_tags,
            }
        }
    }

    impl<F: Field> Circuit<F> for TestCircuit<F> {
        type Config = TestCircuitConfig<F>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            let tx_table = TxTable::construct(meta);
            let rw_table = RwTable::construct(meta);
            let bytecode_table = BytecodeTable::construct(meta);
            let block_table = BlockTable::construct(meta);

            let power_of_randomness = power_of_randomness_from_instance(meta);
            let evm_circuit = EvmCircuit::configure(
                meta,
                power_of_randomness,
                &tx_table,
                &rw_table,
                &bytecode_table,
                &block_table,
            );

            Self::Config {
                tx_table,
                rw_table,
                bytecode_table,
                block_table,
                evm_circuit,
            }
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            config
                .evm_circuit
                .load_fixed_table(&mut layouter, self.fixed_table_tags.clone())?;
            config.evm_circuit.load_byte_table(&mut layouter)?;
            load_txs(
                &config.tx_table,
                &mut layouter,
                &self.block.txs,
                self.block.randomness,
            )?;
            load_rws(
                &config.rw_table,
                &mut layouter,
                &self.block.rws,
                self.block.randomness,
            )?;
            load_bytecodes(
                &config.bytecode_table,
                &mut layouter,
                self.block.bytecodes.iter().map(|(_, b)| b),
                self.block.randomness,
            )?;
            load_block(
                &config.block_table,
                &mut layouter,
                &self.block.context,
                self.block.randomness,
            )?;
            config
                .evm_circuit
                .assign_block_exact(&mut layouter, &self.block)
        }
    }

    impl<F: Field> TestCircuit<F> {
        pub fn get_num_rows_required(block: &Block<F>) -> usize {
            let mut cs = ConstraintSystem::default();
            let config = TestCircuit::configure(&mut cs);
            config.evm_circuit.get_num_rows_required(block)
        }

        pub fn get_active_rows(block: &Block<F>) -> (Vec<usize>, Vec<usize>) {
            let mut cs = ConstraintSystem::default();
            let config = TestCircuit::configure(&mut cs);
            config.evm_circuit.get_active_rows(block)
        }
    }

    pub fn run_test_circuit<F: Field>(
        block: Block<F>,
        fixed_table_tags: Vec<FixedTableTag>,
    ) -> Result<(), Vec<VerifyFailure>> {
        let log2_ceil = |n| u32::BITS - (n as u32).leading_zeros() - (n & (n - 1) == 0) as u32;

        let num_rows_required_for_steps = TestCircuit::get_num_rows_required(&block);

        let k = log2_ceil(
            64 + fixed_table_tags
                .iter()
                .map(|tag| tag.build::<F>().count())
                .sum::<usize>(),
        );
        let k = k.max(log2_ceil(
            64 + block
                .bytecodes
                .values()
                .map(|bytecode| bytecode.bytes.len())
                .sum::<usize>(),
        ));
        let k = k.max(log2_ceil(64 + num_rows_required_for_steps));
        log::debug!("evm circuit uses k = {}", k);

        let power_of_randomness = (1..32)
            .map(|exp| vec![block.randomness.pow(&[exp, 0, 0, 0]); (1 << k) - 64])
            .collect();
        let (active_gate_rows, active_lookup_rows) = TestCircuit::get_active_rows(&block);
        let circuit = TestCircuit::<F>::new(block, fixed_table_tags);
        let prover = MockProver::<F>::run(k, &circuit, power_of_randomness).unwrap();
        prover.verify_at_rows(active_gate_rows.into_iter(), active_lookup_rows.into_iter())
    }

    pub fn run_test_circuit_incomplete_fixed_table<F: Field>(
        block: Block<F>,
    ) -> Result<(), Vec<VerifyFailure>> {
        run_test_circuit(
            block,
            vec![
                FixedTableTag::Zero,
                FixedTableTag::Range5,
                FixedTableTag::Range16,
                FixedTableTag::Range32,
                FixedTableTag::Range64,
                FixedTableTag::Range256,
                FixedTableTag::Range512,
                FixedTableTag::Range1024,
                FixedTableTag::SignByte,
                FixedTableTag::ResponsibleOpcode,
                FixedTableTag::Pow2,
            ],
        )
    }

    pub fn run_test_circuit_complete_fixed_table<F: Field>(
        block: Block<F>,
    ) -> Result<(), Vec<VerifyFailure>> {
        run_test_circuit(block, FixedTableTag::iter().collect())
    }
}
