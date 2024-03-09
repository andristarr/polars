use std::collections::VecDeque;

use arrow::array::Array;
use arrow::bitmap::MutableBitmap;
use polars_error::{polars_bail, PolarsResult};
use polars_utils::slice::GetSaferUnchecked;

use super::super::PagesIter;
use super::utils::{DecodedState, MaybeNext, PageState};
use crate::parquet::encoding::hybrid_rle::HybridRleDecoder;
use crate::parquet::page::{split_buffer, DataPage, DictPage, Page};
use crate::parquet::read::levels::get_bit_width;

/// trait describing deserialized repetition and definition levels
pub trait Nested: std::fmt::Debug + Send + Sync {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>);

    fn push(&mut self, length: i64, is_valid: bool);

    fn is_nullable(&self) -> bool;

    fn is_repeated(&self) -> bool {
        false
    }

    // Whether the Arrow container requires all items to be filled.
    fn is_required(&self) -> bool;

    /// number of rows
    fn len(&self) -> usize;

    /// number of values associated to the primitive type this nested tracks
    fn num_values(&self) -> usize;
}

#[derive(Debug, Default)]
pub struct NestedPrimitive {
    is_nullable: bool,
    length: usize,
}

impl NestedPrimitive {
    pub fn new(is_nullable: bool) -> Self {
        Self {
            is_nullable,
            length: 0,
        }
    }
}

impl Nested for NestedPrimitive {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        (Default::default(), Default::default())
    }

    fn is_nullable(&self) -> bool {
        self.is_nullable
    }

    fn is_required(&self) -> bool {
        false
    }

    fn push(&mut self, _value: i64, _is_valid: bool) {
        self.length += 1
    }

    fn len(&self) -> usize {
        self.length
    }

    fn num_values(&self) -> usize {
        self.length
    }
}

#[derive(Debug, Default)]
pub struct NestedOptional {
    pub validity: MutableBitmap,
    pub offsets: Vec<i64>,
}

impl Nested for NestedOptional {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        let offsets = std::mem::take(&mut self.offsets);
        let validity = std::mem::take(&mut self.validity);
        (offsets, Some(validity))
    }

    fn is_nullable(&self) -> bool {
        true
    }

    fn is_repeated(&self) -> bool {
        true
    }

    fn is_required(&self) -> bool {
        // it may be for FixedSizeList
        false
    }

    fn push(&mut self, value: i64, is_valid: bool) {
        self.offsets.push(value);
        self.validity.push(is_valid);
    }

    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn num_values(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0) as usize
    }
}

impl NestedOptional {
    pub fn with_capacity(capacity: usize) -> Self {
        let offsets = Vec::<i64>::with_capacity(capacity + 1);
        let validity = MutableBitmap::with_capacity(capacity);
        Self { validity, offsets }
    }
}

#[derive(Debug, Default)]
pub struct NestedValid {
    pub offsets: Vec<i64>,
}

impl Nested for NestedValid {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        let offsets = std::mem::take(&mut self.offsets);
        (offsets, None)
    }

    fn is_nullable(&self) -> bool {
        false
    }

    fn is_repeated(&self) -> bool {
        true
    }

    fn is_required(&self) -> bool {
        // it may be for FixedSizeList
        false
    }

    fn push(&mut self, value: i64, _is_valid: bool) {
        self.offsets.push(value);
    }

    fn len(&self) -> usize {
        self.offsets.len()
    }

    fn num_values(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0) as usize
    }
}

impl NestedValid {
    pub fn with_capacity(capacity: usize) -> Self {
        let offsets = Vec::<i64>::with_capacity(capacity + 1);
        Self { offsets }
    }
}

#[derive(Debug, Default)]
pub struct NestedStructValid {
    length: usize,
}

impl NestedStructValid {
    pub fn new() -> Self {
        Self { length: 0 }
    }
}

impl Nested for NestedStructValid {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        (Default::default(), None)
    }

    fn is_nullable(&self) -> bool {
        false
    }

    fn is_required(&self) -> bool {
        true
    }

    fn push(&mut self, _value: i64, _is_valid: bool) {
        self.length += 1;
    }

    fn len(&self) -> usize {
        self.length
    }

    fn num_values(&self) -> usize {
        self.length
    }
}

#[derive(Debug, Default)]
pub struct NestedStruct {
    validity: MutableBitmap,
}

impl NestedStruct {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            validity: MutableBitmap::with_capacity(capacity),
        }
    }
}

impl Nested for NestedStruct {
    fn inner(&mut self) -> (Vec<i64>, Option<MutableBitmap>) {
        (Default::default(), Some(std::mem::take(&mut self.validity)))
    }

    fn is_nullable(&self) -> bool {
        true
    }

    fn is_required(&self) -> bool {
        true
    }

    fn push(&mut self, _value: i64, is_valid: bool) {
        self.validity.push(is_valid)
    }

    fn len(&self) -> usize {
        self.validity.len()
    }

    fn num_values(&self) -> usize {
        self.validity.len()
    }
}

/// A decoder that knows how to map `State` -> Array
pub(super) trait NestedDecoder<'a> {
    type State: PageState<'a>;
    type Dictionary;
    type DecodedState: DecodedState;

    fn build_state(
        &self,
        page: &'a DataPage,
        dict: Option<&'a Self::Dictionary>,
    ) -> PolarsResult<Self::State>;

    /// Initializes a new state
    fn with_capacity(&self, capacity: usize) -> Self::DecodedState;

    fn push_valid(
        &self,
        state: &mut Self::State,
        decoded: &mut Self::DecodedState,
    ) -> PolarsResult<()>;
    fn push_null(&self, decoded: &mut Self::DecodedState);

    fn deserialize_dict(&self, page: &DictPage) -> Self::Dictionary;
}

/// The initial info of nested data types.
/// The `bool` indicates if the type is nullable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitNested {
    /// Primitive data types
    Primitive(bool),
    /// List data types
    List(bool),
    /// Struct data types
    Struct(bool),
}

/// Initialize [`NestedState`] from `&[InitNested]`.
pub fn init_nested(init: &[InitNested], capacity: usize) -> NestedState {
    let container = init
        .iter()
        .map(|init| match init {
            InitNested::Primitive(is_nullable) => {
                Box::new(NestedPrimitive::new(*is_nullable)) as Box<dyn Nested>
            },
            InitNested::List(is_nullable) => {
                if *is_nullable {
                    Box::new(NestedOptional::with_capacity(capacity)) as Box<dyn Nested>
                } else {
                    Box::new(NestedValid::with_capacity(capacity)) as Box<dyn Nested>
                }
            },
            InitNested::Struct(is_nullable) => {
                if *is_nullable {
                    Box::new(NestedStruct::with_capacity(capacity)) as Box<dyn Nested>
                } else {
                    Box::new(NestedStructValid::new()) as Box<dyn Nested>
                }
            },
        })
        .collect();
    NestedState::new(container)
}

pub struct NestedPage<'a> {
    iter: std::iter::Peekable<std::iter::Zip<HybridRleDecoder<'a>, HybridRleDecoder<'a>>>,
}

impl<'a> NestedPage<'a> {
    pub fn try_new(page: &'a DataPage) -> PolarsResult<Self> {
        let (rep_levels, def_levels, _) = split_buffer(page)?;

        let max_rep_level = page.descriptor.max_rep_level;
        let max_def_level = page.descriptor.max_def_level;

        let reps =
            HybridRleDecoder::try_new(rep_levels, get_bit_width(max_rep_level), page.num_values())?;
        let defs =
            HybridRleDecoder::try_new(def_levels, get_bit_width(max_def_level), page.num_values())?;

        let iter = reps.zip(defs).peekable();

        Ok(Self { iter })
    }

    // number of values (!= number of rows)
    pub fn len(&self) -> usize {
        self.iter.size_hint().0
    }
}

/// The state of nested data types.
#[derive(Debug)]
pub struct NestedState {
    /// The nesteds composing `NestedState`.
    pub nested: Vec<Box<dyn Nested>>,
}

impl NestedState {
    /// Creates a new [`NestedState`].
    pub fn new(nested: Vec<Box<dyn Nested>>) -> Self {
        Self { nested }
    }

    /// The number of rows in this state
    pub fn len(&self) -> usize {
        // outermost is the number of rows
        self.nested[0].len()
    }
}

/// Extends `items` by consuming `page`, first trying to complete the last `item`
/// and extending it if more are needed.
///
/// Note that as the page iterator being passed does not guarantee it reads to
/// the end, this function cannot always determine whether it has finished
/// reading. It therefore returns a bool indicating:
/// * true  : the row is fully read
/// * false : the row may not be fully read
pub(super) fn extend<'a, D: NestedDecoder<'a>>(
    page: &'a DataPage,
    init: &[InitNested],
    items: &mut VecDeque<(NestedState, D::DecodedState)>,
    dict: Option<&'a D::Dictionary>,
    remaining: &mut usize,
    decoder: &D,
    chunk_size: Option<usize>,
) -> PolarsResult<bool> {
    let mut values_page = decoder.build_state(page, dict)?;
    let mut page = NestedPage::try_new(page)?;

    debug_assert!(
        items.len() < 2,
        "Should have yielded already completed item before reading more."
    );

    let chunk_size = chunk_size.unwrap_or(usize::MAX);
    let mut first_item_is_fully_read = false;
    // Amortize the allocations.
    let mut cum_sum = vec![];
    let mut cum_rep = vec![];

    loop {
        if let Some((mut nested, mut decoded)) = items.pop_back() {
            let existing = nested.len();
            let additional = (chunk_size - existing).min(*remaining);

            let is_fully_read = extend_offsets2(
                &mut page,
                &mut values_page,
                &mut nested.nested,
                &mut decoded,
                decoder,
                additional,
                &mut cum_sum,
                &mut cum_rep,
            )?;
            first_item_is_fully_read |= is_fully_read;
            *remaining -= nested.len() - existing;
            items.push_back((nested, decoded));

            if page.len() == 0 {
                break;
            }

            if is_fully_read && *remaining == 0 {
                break;
            };
        };

        // At this point:
        // * There are more pages.
        // * The remaining rows have not been fully read.
        // * The deque is empty, or the last item already holds completed data.
        let nested = init_nested(init, chunk_size.min(*remaining));
        let decoded = decoder.with_capacity(0);
        items.push_back((nested, decoded));
    }

    Ok(first_item_is_fully_read)
}

#[allow(clippy::too_many_arguments)]
fn extend_offsets2<'a, D: NestedDecoder<'a>>(
    page: &mut NestedPage<'a>,
    values_state: &mut D::State,
    nested: &mut [Box<dyn Nested>],
    decoded: &mut D::DecodedState,
    decoder: &D,
    additional: usize,
    // Amortized allocations
    cum_sum: &mut Vec<u32>,
    cum_rep: &mut Vec<u32>,
) -> PolarsResult<bool> {
    let max_depth = nested.len();

    cum_sum.resize(max_depth + 1, 0);
    cum_rep.resize(max_depth + 1, 0);
    for (i, nest) in nested.iter().enumerate() {
        let delta = nest.is_nullable() as u32 + nest.is_repeated() as u32;
        unsafe {
            *cum_sum.get_unchecked_release_mut(i + 1) = *cum_sum.get_unchecked_release(i) + delta;
        }
    }

    for (i, nest) in nested.iter().enumerate() {
        let delta = nest.is_repeated() as u32;
        unsafe {
            *cum_rep.get_unchecked_release_mut(i + 1) = *cum_rep.get_unchecked_release(i) + delta;
        }
    }

    let mut rows = 0;
    loop {
        // SAFETY: page.iter is always non-empty on first loop.
        // The current function gets called multiple times with iterators that
        // yield batches of pages. This means e.g. it could be that the very
        // first page is a new row, and the existing nested state has already
        // contains all data from the additional rows.
        if page.iter.peek().unwrap().0 == 0 {
            if rows == additional {
                return Ok(true);
            }
            rows += 1;
        }

        // The errors of the FallibleIterators use in this zipped not checked yet.
        // If one of them errors, the iterator returns None, and this `unwrap` will panic.
        let Some((rep, def)) = page.iter.next() else {
            polars_bail!(ComputeError: "cannot read rep/def levels")
        };

        let mut is_required = false;

        // SAFETY: only bound check elision.
        unsafe {
            for depth in 0..max_depth {
                let right_level = rep <= *cum_rep.get_unchecked_release(depth)
                    && def >= *cum_sum.get_unchecked_release(depth);
                if is_required || right_level {
                    let length = nested
                        .get(depth + 1)
                        .map(|x| x.len() as i64)
                        // the last depth is the leaf, which is always increased by 1
                        .unwrap_or(1);

                    let nest = nested.get_unchecked_release_mut(depth);

                    let is_valid =
                        nest.is_nullable() && def > *cum_sum.get_unchecked_release(depth);
                    nest.push(length, is_valid);
                    is_required = nest.is_required() && !is_valid;

                    if depth == max_depth - 1 {
                        // the leaf / primitive
                        let is_valid =
                            (def != *cum_sum.get_unchecked_release(depth)) || !nest.is_nullable();
                        if right_level && is_valid {
                            decoder.push_valid(values_state, decoded)?;
                        } else {
                            decoder.push_null(decoded);
                        }
                    }
                }
            }
        }

        if page.iter.len() == 0 {
            return Ok(false);
        }
    }
}

#[inline]
pub(super) fn next<'a, I, D>(
    iter: &'a mut I,
    items: &mut VecDeque<(NestedState, D::DecodedState)>,
    dict: &'a mut Option<D::Dictionary>,
    remaining: &mut usize,
    init: &[InitNested],
    chunk_size: Option<usize>,
    decoder: &D,
) -> MaybeNext<PolarsResult<(NestedState, D::DecodedState)>>
where
    I: PagesIter,
    D: NestedDecoder<'a>,
{
    // front[a1, a2, a3, ...]back
    if items.len() > 1 {
        return MaybeNext::Some(Ok(items.pop_front().unwrap()));
    }

    match iter.next() {
        Err(e) => MaybeNext::Some(Err(e.into())),
        Ok(None) => {
            if let Some(decoded) = items.pop_front() {
                MaybeNext::Some(Ok(decoded))
            } else {
                MaybeNext::None
            }
        },
        Ok(Some(page)) => {
            let page = match page {
                Page::Data(page) => page,
                Page::Dict(dict_page) => {
                    *dict = Some(decoder.deserialize_dict(dict_page));
                    return MaybeNext::More;
                },
            };

            // there is a new page => consume the page from the start
            let is_fully_read = extend(
                page,
                init,
                items,
                dict.as_ref(),
                remaining,
                decoder,
                chunk_size,
            );

            match is_fully_read {
                Ok(true) => MaybeNext::Some(Ok(items.pop_front().unwrap())),
                Ok(false) => MaybeNext::More,
                Err(e) => MaybeNext::Some(Err(e)),
            }
        },
    }
}

/// Type def for a sharable, boxed dyn [`Iterator`] of NestedStates and arrays
pub type NestedArrayIter<'a> =
    Box<dyn Iterator<Item = PolarsResult<(NestedState, Box<dyn Array>)>> + Send + Sync + 'a>;
