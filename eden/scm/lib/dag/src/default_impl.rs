/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;

use futures::future::BoxFuture;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryStreamExt;

use crate::namedag::MemNameDag;
use crate::nameset::hints::Hints;
use crate::ops::DagAddHeads;
use crate::ops::Parents;
use crate::DagAlgorithm;
use crate::NameSet;
use crate::Result;
use crate::VertexName;

/// Re-create the graph so it looks better when rendered.
///
/// For example, the left-side graph will be rewritten to the right-side:
///
/// 1. Linearize.
///
/// ```plain,ignore
///   A             A      # Linearize is done by IdMap::assign_heads,
///   |             |      # as long as the heads provided are the heads
///   | C           B      # of the whole graph ("A", "C", not "B", "D").
///   | |           |
///   B |     ->    | C
///   | |           | |
///   | D           | D
///   |/            |/
///   E             E
/// ```
///
/// 2. Reorder branches (at different branching points) to reduce columns.
///
/// ```plain,ignore
///     D           B
///     |           |      # Assuming the main branch is B-C-E.
///   B |           | A    # Branching point of the D branch is "C"
///   | |           |/     # Branching point of the A branch is "C"
///   | | A   ->    C      # The D branch should be moved to below
///   | |/          |      # the A branch.
///   | |           | D
///   |/|           |/
///   C /           E
///   |/
///   E
/// ```
///
/// 3. Reorder branches (at a same branching point) to reduce length of
///    edges.
///
/// ```plain,ignore
///   D              A
///   |              |     # This is done by picking the longest
///   | A            B     # branch (A-B-C-E) as the "main branch"
///   | |            |     # and work on the remaining branches
///   | B     ->     C     # recursively.
///   | |            |
///   | C            | D
///   |/             |/
///   E              E
/// ```
///
/// `main_branch` optionally defines how to sort the heads. A head `x` will
/// be emitted first during iteration, if `ancestors(x) & main_branch`
/// contains larger vertexes. For example, if `main_branch` is `[C, D, E]`,
/// then `C` will be emitted first, and the returned DAG will have `all()`
/// output `[C, D, A, B, E]`. Practically, `main_branch` usually contains
/// "public" commits.
///
/// This function is expensive. Only run on small graphs.
///
/// This function is currently more optimized for "forking" cases. It is
/// not yet optimized for graphs with many merges.
pub(crate) async fn beautify(
    this: &(impl DagAlgorithm + ?Sized),
    main_branch: Option<NameSet>,
) -> Result<MemNameDag> {
    // Find the "largest" branch.
    async fn find_main_branch<F, O>(get_ancestors: &F, heads: &[VertexName]) -> Result<NameSet>
    where
        F: Fn(&VertexName) -> O,
        F: Send,
        O: Future<Output = Result<NameSet>>,
        O: Send,
    {
        let mut best_branch = NameSet::empty();
        let mut best_count = best_branch.count().await?;
        for head in heads {
            let branch = get_ancestors(head).await?;
            let count = branch.count().await?;
            if count > best_count {
                best_count = count;
                best_branch = branch;
            }
        }
        Ok(best_branch)
    }

    // Sort heads recursively.
    // Cannot use "async fn" due to rustc limitation on async recursion.
    fn sort<'a: 't, 'b: 't, 't, F, O>(
        get_ancestors: &'a F,
        heads: &'b mut [VertexName],
        main_branch: NameSet,
    ) -> BoxFuture<'t, Result<()>>
    where
        F: Fn(&VertexName) -> O,
        F: Send + Sync,
        O: Future<Output = Result<NameSet>>,
        O: Send,
    {
        let fut = async move {
            if heads.len() <= 1 {
                return Ok(());
            }

            // Sort heads by "branching point" on the main branch.
            let mut branching_points: HashMap<VertexName, usize> =
                HashMap::with_capacity(heads.len());

            for head in heads.iter() {
                let count = (get_ancestors(head).await? & main_branch.clone())
                    .count()
                    .await?;
                branching_points.insert(head.clone(), count);
            }
            heads.sort_by_key(|v| branching_points.get(v));

            // For heads with a same branching point, sort them recursively
            // using a different "main branch".
            let mut start = 0;
            let mut start_branching_point: Option<usize> = None;
            for end in 0..=heads.len() {
                let branching_point = heads
                    .get(end)
                    .and_then(|h| branching_points.get(&h).cloned());
                if branching_point != start_branching_point {
                    if start + 1 < end {
                        let heads = &mut heads[start..end];
                        let main_branch = find_main_branch(get_ancestors, heads).await?;
                        // "boxed" is used to workaround async recursion.
                        sort(get_ancestors, heads, main_branch).boxed().await?;
                    }
                    start = end;
                    start_branching_point = branching_point;
                }
            }

            Ok(())
        };
        Box::pin(fut)
    }

    let main_branch = main_branch.unwrap_or_else(NameSet::empty);
    let heads = this
        .heads_ancestors(this.all().await?)
        .await?
        .iter()
        .await?;
    let mut heads: Vec<_> = heads.try_collect().await?;
    let get_ancestors = |head: &VertexName| this.ancestors(head.into());
    // Stabilize output if the sort key conflicts.
    heads.sort();
    sort(&get_ancestors, &mut heads[..], main_branch).await?;

    let mut dag = MemNameDag::new();
    dag.add_heads(&this.dag_snapshot()?, &heads).await?;
    Ok(dag)
}

pub(crate) async fn parents(this: &(impl DagAlgorithm + ?Sized), set: NameSet) -> Result<NameSet> {
    let mut result: Vec<VertexName> = Vec::new();
    let mut iter = set.iter().await?;
    // PERF: This is not an efficient async implementation.
    while let Some(vertex) = iter.next().await {
        let parents = this.parent_names(vertex?).await?;
        result.extend(parents);
    }
    Ok(NameSet::from_static_names(result))
}

pub(crate) async fn first_ancestor_nth(
    this: &(impl DagAlgorithm + ?Sized),
    name: VertexName,
    n: u64,
) -> Result<Option<VertexName>> {
    let mut vertex = name.clone();
    for _ in 0..n {
        let parents = this.parent_names(vertex).await?;
        if parents.is_empty() {
            return Ok(None);
        }
        vertex = parents[0].clone();
    }
    Ok(Some(vertex))
}

pub(crate) async fn first_ancestors(
    this: &(impl DagAlgorithm + ?Sized),
    set: NameSet,
) -> Result<NameSet> {
    let mut to_visit: Vec<VertexName> = {
        let mut list = Vec::with_capacity(set.count().await?);
        let mut iter = set.iter().await?;
        while let Some(next) = iter.next().await {
            let vertex = next?;
            list.push(vertex);
        }
        list
    };
    let mut visited: HashSet<VertexName> = to_visit.clone().into_iter().collect();
    while let Some(v) = to_visit.pop() {
        #[allow(clippy::never_loop)]
        if let Some(parent) = this.parent_names(v).await?.into_iter().next() {
            if visited.insert(parent.clone()) {
                to_visit.push(parent);
            }
        }
    }
    let hints = Hints::new_inherit_idmap_dag(set.hints());
    let set = NameSet::from_iter(visited.into_iter().map(Ok), hints);
    this.sort(&set).await
}

pub(crate) async fn heads(this: &(impl DagAlgorithm + ?Sized), set: NameSet) -> Result<NameSet> {
    Ok(set.clone() - this.parents(set).await?)
}

pub(crate) async fn roots(this: &(impl DagAlgorithm + ?Sized), set: NameSet) -> Result<NameSet> {
    Ok(set.clone() - this.children(set).await?)
}

pub(crate) async fn merges(this: &(impl DagAlgorithm + ?Sized), set: NameSet) -> Result<NameSet> {
    let this = this.dag_snapshot()?;
    Ok(set.filter(Box::new(move |v: &VertexName| {
        let this = this.clone();
        Box::pin(async move {
            DagAlgorithm::parent_names(&this, v.clone())
                .await
                .map(|ps| ps.len() >= 2)
        })
    })))
}

pub(crate) async fn reachable_roots(
    this: &(impl DagAlgorithm + ?Sized),
    roots: NameSet,
    heads: NameSet,
) -> Result<NameSet> {
    let heads_ancestors = this.ancestors(heads.clone()).await?;
    let roots = roots & heads_ancestors.clone(); // Filter out "bogus" roots.
    let only = heads_ancestors - this.ancestors(roots.clone()).await?;
    Ok(roots.clone() & (heads.clone() | this.parents(only).await?))
}

pub(crate) async fn heads_ancestors(
    this: &(impl DagAlgorithm + ?Sized),
    set: NameSet,
) -> Result<NameSet> {
    this.heads(this.ancestors(set).await?).await
}

pub(crate) async fn only(
    this: &(impl DagAlgorithm + ?Sized),
    reachable: NameSet,
    unreachable: NameSet,
) -> Result<NameSet> {
    let reachable = this.ancestors(reachable).await?;
    let unreachable = this.ancestors(unreachable).await?;
    Ok(reachable - unreachable)
}

pub(crate) async fn only_both(
    this: &(impl DagAlgorithm + ?Sized),
    reachable: NameSet,
    unreachable: NameSet,
) -> Result<(NameSet, NameSet)> {
    let reachable = this.ancestors(reachable).await?;
    let unreachable = this.ancestors(unreachable).await?;
    Ok((reachable - unreachable.clone(), unreachable))
}

pub(crate) async fn gca_one(
    this: &(impl DagAlgorithm + ?Sized),
    set: NameSet,
) -> Result<Option<VertexName>> {
    this.gca_all(set)
        .await?
        .iter()
        .await?
        .next()
        .await
        .transpose()
}

pub(crate) async fn gca_all(this: &(impl DagAlgorithm + ?Sized), set: NameSet) -> Result<NameSet> {
    this.heads_ancestors(this.common_ancestors(set).await?)
        .await
}

pub(crate) async fn common_ancestors(
    this: &(impl DagAlgorithm + ?Sized),
    set: NameSet,
) -> Result<NameSet> {
    let result = match set.count().await? {
        0 => set,
        1 => this.ancestors(set).await?,
        _ => {
            // Try to reduce the size of `set`.
            // `common_ancestors(X)` = `common_ancestors(roots(X))`.
            let set = this.roots(set).await?;
            let mut iter = set.iter().await?;
            let mut result = this
                .ancestors(NameSet::from(iter.next().await.unwrap()?))
                .await?;
            while let Some(v) = iter.next().await {
                result = result.intersection(&this.ancestors(NameSet::from(v?)).await?);
            }
            result
        }
    };
    Ok(result)
}

pub(crate) async fn is_ancestor(
    this: &(impl DagAlgorithm + ?Sized),
    ancestor: VertexName,
    descendant: VertexName,
) -> Result<bool> {
    let mut to_visit = vec![descendant];
    let mut visited: HashSet<_> = to_visit.clone().into_iter().collect();
    while let Some(v) = to_visit.pop() {
        if v == ancestor {
            return Ok(true);
        }
        for parent in this.parent_names(v).await? {
            if visited.insert(parent.clone()) {
                to_visit.push(parent);
            }
        }
    }
    Ok(false)
}

#[tracing::instrument(skip(this), level=tracing::Level::DEBUG)]
pub(crate) async fn hint_subdag_for_insertion(
    this: &(impl Parents + ?Sized),
    scope: &NameSet,
    heads: &[VertexName],
) -> Result<MemNameDag> {
    let count = scope.count().await?;
    tracing::trace!("hint_subdag_for_insertion: pending vertexes: {}", count);

    // ScopedParents only contains parents within "scope".
    struct ScopedParents<'a, P: Parents + ?Sized> {
        parents: &'a P,
        scope: &'a NameSet,
    }

    #[async_trait::async_trait]
    impl<'a, P: Parents + ?Sized> Parents for ScopedParents<'a, P> {
        async fn parent_names(&self, name: VertexName) -> Result<Vec<VertexName>> {
            let parents: Vec<VertexName> = self.parents.parent_names(name).await?;
            // Filter by scope. We don't need to provide a "correct" parents here.
            // It is only used to optimize network fetches, not used to actually insert
            // to the graph.
            let mut filtered_parents = Vec::with_capacity(parents.len());
            for v in parents {
                if self.scope.contains(&v).await? {
                    filtered_parents.push(v)
                }
            }
            Ok(filtered_parents)
        }

        async fn hint_subdag_for_insertion(&self, _heads: &[VertexName]) -> Result<MemNameDag> {
            // No need to use such a hint (to avoid infinite recursion).
            // Pending names should exist in the graph without using remote fetching.
            Ok(MemNameDag::new())
        }
    }

    // Insert vertexes in `scope` to `dag`.
    let mut dag = MemNameDag::new();
    // The MemNameDag should not be lazy.
    assert!(!dag.is_vertex_lazy());

    let scoped_parents = ScopedParents {
        parents: this,
        scope,
    };
    dag.add_heads(&scoped_parents, heads).await?;

    Ok(dag)
}
