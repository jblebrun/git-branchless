//! Deal with Git's garbage collection mechanism.
//!
//! Git treats a commit as unreachable if there are no references that point to
//! it or one of its descendants. However, the branchless workflow oftentimes
//! involves keeping such commits reachable until the user has explicitly hidden
//! them.
//!
//! This module is responsible for adding extra references to Git, so that Git's
//! garbage collection doesn't collect commits which branchless thinks are still
//! visible.

use std::io::Write;

use anyhow::Context;
use pyo3::prelude::*;

use crate::eventlog::{is_gc_ref, EventLogDb, EventReplayer};
use crate::graph::{make_graph, BranchOids, CommitGraph, HeadOid, MainBranchOid};
use crate::mergebase::MergeBaseDb;
use crate::python::{clone_conn, make_repo_from_py_repo, map_err_to_py_err, PyOid, TextIO};
use crate::util::{
    get_branch_oid_to_names, get_db_conn, get_head_oid, get_main_branch_oid, get_repo,
};

fn find_dangling_references<'repo>(
    repo: &'repo git2::Repository,
    graph: &CommitGraph,
) -> anyhow::Result<Vec<git2::Reference<'repo>>> {
    let references = repo
        .references()
        .with_context(|| "Getting repo references")?;

    let mut result = Vec::new();
    for reference in references {
        let reference = reference.with_context(|| "Reading reference info")?;
        let reference_name = match reference.name() {
            Some(name) => name.to_owned(),
            None => continue,
        };
        let resolved_reference = reference
            .resolve()
            .with_context(|| format!("Resolving reference: {}", reference_name))?;

        // The graph only contains commits, so we don't need to handle the
        // case of the reference not peeling to a valid commit. (It might be
        // a reference to a different kind of object.)
        if let Ok(commit) = resolved_reference.peel_to_commit() {
            if is_gc_ref(&reference_name) && !graph.contains_key(&commit.id()) {
                result.push(reference)
            }
        }
    }
    Ok(result)
}

/// Mark a commit as reachable.
///
/// Once marked as reachable, the commit won't be collected by Git's garbage
/// collection mechanism until first garbage-collected by branchless itself
/// (using the `gc` function).
///
/// Args:
/// * `repo`: The Git repository.
/// * `commit_oid`: The commit OID to mark as reachable.
pub fn mark_commit_reachable(repo: &git2::Repository, commit_oid: git2::Oid) -> anyhow::Result<()> {
    let ref_name = format!("refs/branchless/{}", commit_oid.to_string());
    anyhow::ensure!(
        git2::Reference::is_valid_name(&ref_name),
        format!("Invalid ref name to mark commit as reachable: {}", ref_name)
    );
    repo.reference(
        &ref_name,
        commit_oid,
        true,
        "branchless: marking commit as reachable",
    )
    .with_context(|| format!("Creating reference {}", ref_name))?;
    Ok(())
}

/// Run branchless's garbage collection.
///
/// Frees any references to commits which are no longer visible in the smartlog.
pub fn gc<Out: Write>(out: &mut Out) -> anyhow::Result<()> {
    let repo = get_repo()?;
    let conn = get_db_conn(&repo)?;
    let merge_base_db = MergeBaseDb::new(clone_conn(&conn)?)?;
    let event_log_db = EventLogDb::new(clone_conn(&conn)?)?;
    let event_replayer = EventReplayer::from_event_log_db(&event_log_db)?;
    let head_oid = get_head_oid(&repo)?;
    let main_branch_oid = get_main_branch_oid(&repo)?;
    let branch_oid_to_names = get_branch_oid_to_names(&repo)?;

    let graph = make_graph(
        &repo,
        &merge_base_db,
        &event_replayer,
        &HeadOid(head_oid),
        &MainBranchOid(main_branch_oid),
        &BranchOids(branch_oid_to_names.keys().copied().collect()),
        true,
    )?;

    writeln!(out, "branchless: collecting garbage")?;
    let dangling_references = find_dangling_references(&repo, &graph)?;
    for mut reference in dangling_references.into_iter() {
        reference
            .delete()
            .with_context(|| format!("Deleting reference {:?}", reference.name()))?;
    }
    Ok(())
}

#[pyfunction]
fn py_mark_commit_reachable(py: Python, repo: PyObject, commit_oid: PyOid) -> PyResult<()> {
    let repo = make_repo_from_py_repo(py, &repo)?;
    let PyOid(commit_oid) = commit_oid;
    map_err_to_py_err(
        mark_commit_reachable(&repo, commit_oid),
        "Could not mark commit as reachable",
    )?;
    Ok(())
}

#[pyfunction]
fn py_gc(py: Python, out: PyObject) -> PyResult<()> {
    let mut text_io = TextIO::new(py, out);
    map_err_to_py_err(gc(&mut text_io), "Failed to run GC")?;
    Ok(())
}

pub fn register_python_symbols(module: &PyModule) -> PyResult<()> {
    module.add_function(pyo3::wrap_pyfunction!(py_mark_commit_reachable, module)?)?;
    module.add_function(pyo3::wrap_pyfunction!(py_gc, module)?)?;
    Ok(())
}