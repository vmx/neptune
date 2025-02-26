use crate::batch_hasher::Batcher;
use crate::error::Error;
use crate::poseidon::{Poseidon, PoseidonConstants};
use crate::tree_builder::{TreeBuilder, TreeBuilderTrait};
use crate::{Arity, BatchHasher, NeptuneField};
use ff::{Field, PrimeField};
use generic_array::{ArrayLength, GenericArray};

pub trait ColumnTreeBuilderTrait<F, ColumnArity, TreeArity>
where
    F: PrimeField,
    ColumnArity: Arity<F>,
    TreeArity: Arity<F>,
{
    fn add_columns(&mut self, columns: &[GenericArray<F, ColumnArity>]) -> Result<(), Error>;
    fn add_final_columns(
        &mut self,
        columns: &[GenericArray<F, ColumnArity>],
    ) -> Result<(Vec<F>, Vec<F>), Error>;

    fn reset(&mut self);
}

pub struct ColumnTreeBuilder<F, ColumnArity, TreeArity>
where
    F: NeptuneField,
    ColumnArity: Arity<F>,
    TreeArity: Arity<F>,
{
    pub leaf_count: usize,
    data: Vec<F>,
    /// Index of the first unfilled datum.
    fill_index: usize,
    column_constants: PoseidonConstants<F, ColumnArity>,
    pub column_batcher: Option<Batcher<F, ColumnArity>>,
    tree_builder: TreeBuilder<F, TreeArity>,
}

impl<F, ColumnArity, TreeArity> ColumnTreeBuilderTrait<F, ColumnArity, TreeArity>
    for ColumnTreeBuilder<F, ColumnArity, TreeArity>
where
    F: NeptuneField,
    ColumnArity: Arity<F>,
    TreeArity: Arity<F>,
{
    fn add_columns(&mut self, columns: &[GenericArray<F, ColumnArity>]) -> Result<(), Error> {
        let start = self.fill_index;
        let column_count = columns.len();
        let end = start + column_count;

        if end > self.leaf_count {
            return Err(Error::Other("too many columns".to_string()));
        }

        match self.column_batcher {
            Some(ref mut batcher) => {
                batcher.hash_into_slice(&mut self.data[start..start + column_count], columns)?;
            }
            None => columns.iter().enumerate().for_each(|(i, column)| {
                self.data[start + i] =
                    Poseidon::new_with_preimage(column, &self.column_constants).hash();
            }),
        };

        self.fill_index += column_count;

        Ok(())
    }

    fn add_final_columns(
        &mut self,
        columns: &[GenericArray<F, ColumnArity>],
    ) -> Result<(Vec<F>, Vec<F>), Error> {
        self.add_columns(columns)?;

        let (base, tree) = self.tree_builder.add_final_leaves(&self.data)?;
        self.reset();

        Ok((base, tree))
    }

    fn reset(&mut self) {
        self.fill_index = 0;
        self.data.iter_mut().for_each(|place| *place = F::zero());
    }
}
fn as_generic_arrays<A: Arity<F>, F: PrimeField>(vec: &[F]) -> &[GenericArray<F, A>] {
    // It is a programmer error to call `as_generic_arrays` on a vector whose underlying data cannot be divided
    // into an even number of `GenericArray<Fr, Arity>`.
    assert_eq!(
        0,
        (vec.len() * std::mem::size_of::<F>()) % std::mem::size_of::<GenericArray<F, A>>()
    );

    // This block does not affect the underlying `Fr`s. It just groups them into `GenericArray`s of length `Arity`.
    // We know by the assertion above that `vec` can be evenly divided into these units.
    unsafe {
        std::slice::from_raw_parts(
            vec.as_ptr() as *const () as *const GenericArray<F, A>,
            vec.len() / A::to_usize(),
        )
    }
}

impl<F, ColumnArity, TreeArity> ColumnTreeBuilder<F, ColumnArity, TreeArity>
where
    F: NeptuneField,
    ColumnArity: Arity<F>,
    TreeArity: Arity<F>,
{
    pub fn new(
        column_batcher: Option<Batcher<F, ColumnArity>>,
        tree_batcher: Option<Batcher<F, TreeArity>>,
        leaf_count: usize,
    ) -> Result<Self, Error> {
        let tree_builder = TreeBuilder::<F, TreeArity>::new(tree_batcher, leaf_count, 0)?;

        let builder = Self {
            leaf_count,
            data: vec![F::zero(); leaf_count],
            fill_index: 0,
            column_constants: PoseidonConstants::<F, ColumnArity>::new(),
            column_batcher,
            tree_builder,
        };

        Ok(builder)
    }

    pub fn tree_size(&self) -> usize {
        self.tree_builder.tree_size(0)
    }

    // Compute root of tree composed of all identical columns. For use in checking correctness of GPU column tree-building
    // without the cost of generating a full column tree.
    pub fn compute_uniform_tree_root(
        &mut self,
        column: GenericArray<F, ColumnArity>,
    ) -> Result<F, Error> {
        // All the leaves will be the same.
        let element = Poseidon::new_with_preimage(&column, &self.column_constants).hash();

        self.tree_builder.compute_uniform_tree_root(element)
    }
}

#[cfg(all(any(feature = "cuda", feature = "opencl"), not(target_os = "macos")))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::poseidon::{Arity, Poseidon};
    use crate::BatchHasher;
    use blstrs::Scalar as Fr;
    use ff::Field;
    use generic_array::sequence::GenericSequence;
    use generic_array::typenum::{U11, U8};

    #[test]
    fn test_column_tree_builder() {
        // 16KiB tree has 512 leaves.
        test_column_tree_builder_aux(None, None, 512, 32);
        test_column_tree_builder_aux(
            Some(Batcher::new_cpu(512)),
            Some(Batcher::new_cpu(512)),
            512,
            32,
        );

        test_column_tree_builder_aux(
            Some(Batcher::pick_gpu(512).unwrap()),
            Some(Batcher::pick_gpu(512).unwrap()),
            512,
            32,
        );
    }

    fn test_column_tree_builder_aux(
        column_batcher: Option<Batcher<Fr, U11>>,
        tree_batcher: Option<Batcher<Fr, U8>>,
        leaves: usize,
        num_batches: usize,
    ) {
        let batch_size = leaves / num_batches;

        let mut builder =
            ColumnTreeBuilder::<Fr, U11, U8>::new(column_batcher, tree_batcher, leaves).unwrap();

        // Simplify computing the expected root.
        let constant_element = Fr::zero();
        let constant_column = GenericArray::<Fr, U11>::generate(|_| constant_element);

        let max_batch_size = if let Some(batcher) = &builder.column_batcher {
            batcher.max_batch_size()
        } else {
            leaves
        };

        let effective_batch_size = usize::min(batch_size, max_batch_size);

        let mut total_columns = 0;
        while total_columns + effective_batch_size < leaves {
            let columns: Vec<GenericArray<Fr, U11>> =
                (0..effective_batch_size).map(|_| constant_column).collect();

            builder.add_columns(columns.as_slice()).unwrap();
            total_columns += columns.len();
        }

        let final_columns: Vec<_> = (0..leaves - total_columns)
            .map(|_| GenericArray::<Fr, U11>::generate(|_| constant_element))
            .collect();

        let (base, res) = builder.add_final_columns(final_columns.as_slice()).unwrap();

        let column_hash =
            Poseidon::new_with_preimage(&constant_column, &builder.column_constants).hash();
        assert!(base.iter().all(|x| *x == column_hash));

        let computed_root = res[res.len() - 1];

        let expected_root = builder.compute_uniform_tree_root(final_columns[0]).unwrap();
        let expected_size = builder.tree_builder.tree_size(0);

        assert_eq!(leaves, base.len());
        assert_eq!(expected_size, res.len());
        assert_eq!(expected_root, computed_root);
    }
}
