// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::binary_heap::PeekMut;
use std::collections::{BinaryHeap, LinkedList};
use std::sync::Arc;

use async_trait::async_trait;
use risingwave_hummock_sdk::VersionedComparator;

use crate::hummock::iterator::{
    BoxedHummockIterator, DirectionEnum, HummockIterator, HummockIteratorDirection,
};
use crate::hummock::value::HummockValue;
use crate::hummock::HummockResult;
use crate::monitor::StateStoreMetrics;

pub trait NodeExtraOrderInfo: Eq + Ord + Send + Sync {}

/// For unordered merge iterator, no extra order info is needed.
type UnorderedNodeExtra = ();
/// Store the order index for the order aware merge iterator
type OrderedNodeExtra = usize;
impl NodeExtraOrderInfo for UnorderedNodeExtra {}
impl NodeExtraOrderInfo for OrderedNodeExtra {}

pub struct Node<'a, D: HummockIteratorDirection, T: NodeExtraOrderInfo> {
    iter: BoxedHummockIterator<'a, D>,
    extra_order_info: T,
}

impl<T: NodeExtraOrderInfo, D: HummockIteratorDirection> Eq for Node<'_, D, T> where Self: PartialEq {}
impl<T: NodeExtraOrderInfo, D: HummockIteratorDirection> Ord for Node<'_, D, T>
where
    Self: PartialOrd,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

/// Implement `PartialOrd` for unordered iter node. Only compare the key.
impl<D: HummockIteratorDirection> PartialOrd for Node<'_, D, UnorderedNodeExtra> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Note: to implement min-heap by using max-heap internally, the comparing
        // order should be reversed.

        Some(match D::direction() {
            DirectionEnum::Forward => {
                VersionedComparator::compare_key(other.iter.key(), self.iter.key())
            }
            DirectionEnum::Backward => {
                VersionedComparator::compare_key(self.iter.key(), other.iter.key())
            }
        })
    }
}

/// Implement `PartialOrd` for ordered iter node. Compare key and use order index as tie breaker.
impl<D: HummockIteratorDirection> PartialOrd for Node<'_, D, OrderedNodeExtra> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // The `extra_info` is used as a tie-breaker when the keys are equal.
        Some(match D::direction() {
            DirectionEnum::Forward => {
                VersionedComparator::compare_key(other.iter.key(), self.iter.key())
                    .then_with(|| other.extra_order_info.cmp(&self.extra_order_info))
            }
            DirectionEnum::Backward => {
                VersionedComparator::compare_key(self.iter.key(), other.iter.key())
                    .then_with(|| self.extra_order_info.cmp(&other.extra_order_info))
            }
        })
    }
}

impl<D: HummockIteratorDirection> PartialEq for Node<'_, D, UnorderedNodeExtra> {
    fn eq(&self, other: &Self) -> bool {
        self.iter.key() == other.iter.key()
    }
}

impl<D: HummockIteratorDirection> PartialEq for Node<'_, D, OrderedNodeExtra> {
    fn eq(&self, other: &Self) -> bool {
        self.iter.key() == other.iter.key() && self.extra_order_info.eq(&other.extra_order_info)
    }
}

/// Iterates on multiple iterators, a.k.a. `MergeIterator`.
pub struct MergeIteratorInner<'a, D: HummockIteratorDirection, NE: NodeExtraOrderInfo> {
    /// Invalid or non-initialized iterators.
    unused_iters: LinkedList<Node<'a, D, NE>>,

    /// The heap for merge sort.
    heap: BinaryHeap<Node<'a, D, NE>>,

    /// Statistics.
    stats: Arc<StateStoreMetrics>,

    last_table_key: Vec<u8>,
}

/// An order aware merge iterator.
pub type OrderedMergeIteratorInner<'a, D> = MergeIteratorInner<'a, D, OrderedNodeExtra>;

impl<'a, D: HummockIteratorDirection> OrderedMergeIteratorInner<'a, D> {
    #[allow(dead_code)]
    pub fn new(
        iterators: impl IntoIterator<Item = BoxedHummockIterator<'a, D>>,
        stats: Arc<StateStoreMetrics>,
    ) -> Self {
        Self {
            unused_iters: iterators
                .into_iter()
                .enumerate()
                .map(|(i, iter)| Node {
                    iter,
                    extra_order_info: i,
                })
                .collect(),
            heap: BinaryHeap::new(),
            stats,
            last_table_key: vec![],
        }
    }
}

pub type UnorderedMergeIteratorInner<'a, D> = MergeIteratorInner<'a, D, UnorderedNodeExtra>;

impl<'a, D: HummockIteratorDirection> UnorderedMergeIteratorInner<'a, D> {
    pub fn new(
        iterators: impl IntoIterator<Item = BoxedHummockIterator<'a, D>>,
        stats: Arc<StateStoreMetrics>,
    ) -> Self {
        Self {
            unused_iters: iterators
                .into_iter()
                .map(|iter| Node {
                    iter,
                    extra_order_info: (),
                })
                .collect(),
            heap: BinaryHeap::new(),
            stats,
            last_table_key: vec![],
        }
    }
}

impl<'a, D: HummockIteratorDirection, NE: NodeExtraOrderInfo> MergeIteratorInner<'a, D, NE>
where
    Node<'a, D, NE>: Ord,
{
    /// Moves all iterators from the `heap` to the linked list.
    fn reset_heap(&mut self) {
        self.unused_iters.extend(self.heap.drain());
    }

    /// After some iterators in `unused_iterators` are sought or rewound, calls this function
    /// to construct a new heap using the valid ones.
    fn build_heap(&mut self) {
        assert!(self.heap.is_empty());

        self.heap = self
            .unused_iters
            .drain_filter(|i| i.iter.is_valid())
            .collect();
    }
}

#[async_trait]
/// The behaviour of `next` of order aware merge iterator is different from the normal one, so we
/// extract this trait.
trait MergeIteratorNext {
    async fn next_inner(&mut self) -> HummockResult<()>;
}

#[async_trait]
impl<'a, D: HummockIteratorDirection> MergeIteratorNext for OrderedMergeIteratorInner<'a, D> {
    async fn next_inner(&mut self) -> HummockResult<()> {
        let top_key = {
            let top_key = self.heap.peek().expect("no inner iter").iter.key();
            self.last_table_key.clear();
            self.last_table_key
                .extend_from_slice(top_key);
            self.last_table_key.as_slice()
        };
        loop {
            let mut node = match self.heap.peek_mut() {
                None => {
                    break;
                }
                Some(node) => node,
            };
            // WARNING: within scope of BinaryHeap::PeekMut, we must carefully handle all places
            // of return. Once the iterator enters an invalid state, we should
            // remove it from heap before returning.

            if node.iter.key() == top_key {
                if let Err(e) = node.iter.next().await {
                    let _node = PeekMut::pop(node);
                    self.heap.clear();
                    return Err(e);
                };
                if !node.iter.is_valid() {
                    let node = PeekMut::pop(node);
                    self.unused_iters.push_back(node);
                } else {
                    drop(node);
                }
            } else {
                drop(node);
                break;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl<'a, D: HummockIteratorDirection> MergeIteratorNext for UnorderedMergeIteratorInner<'a, D> {
    async fn next_inner(&mut self) -> HummockResult<()> {
        let mut node = self.heap.peek_mut().expect("no inner iter");

        // WARNING: within scope of BinaryHeap::PeekMut, we must carefully handle all places of
        // return. Once the iterator enters an invalid state, we should remove it from heap
        // before returning.

        match node.iter.next().await {
            Ok(_) => {}
            Err(e) => {
                // If the iterator returns error, we should clear the heap, so that this iterator
                // becomes invalid.
                PeekMut::pop(node);
                self.heap.clear();
                return Err(e);
            }
        }

        if !node.iter.is_valid() {
            // Put back to `unused_iters`
            let node = PeekMut::pop(node);
            self.unused_iters.push_back(node);
        } else {
            // This will update the heap top.
            drop(node);
        }

        Ok(())
    }
}

#[async_trait]
impl<'a, D: HummockIteratorDirection, NE: NodeExtraOrderInfo> HummockIterator
    for MergeIteratorInner<'a, D, NE>
where
    Self: MergeIteratorNext,
    Node<'a, D, NE>: Ord,
{
    type Direction = D;

    async fn next(&mut self) -> HummockResult<()> {
        self.next_inner().await
    }

    fn key(&self) -> &[u8] {
        self.heap.peek().expect("no inner iter").iter.key()
    }

    fn value(&self) -> HummockValue<&[u8]> {
        self.heap.peek().expect("no inner iter").iter.value()
    }

    fn is_valid(&self) -> bool {
        self.heap.peek().map_or(false, |n| n.iter.is_valid())
    }

    async fn rewind(&mut self) -> HummockResult<()> {
        self.reset_heap();
        futures::future::try_join_all(self.unused_iters.iter_mut().map(|x| x.iter.rewind()))
            .await?;
        self.build_heap();
        Ok(())
    }

    async fn seek(&mut self, key: &[u8]) -> HummockResult<()> {
        let timer = self.stats.iter_merge_seek_duration.start_timer();

        self.reset_heap();
        futures::future::try_join_all(self.unused_iters.iter_mut().map(|x| x.iter.seek(key)))
            .await?;
        self.build_heap();

        timer.observe_duration();
        Ok(())
    }
}
