/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use anyhow::Context;
use async_runtime::block_unless_interrupted as block_on;
use cliparser::define_flags;
use dag::namedag::IndexedLogNameDagPath;
use dag::ops::DagImportPullData;
use dag::ops::DagPersistent;
use dag::ops::Open;
use dag::CloneData;
use dag::VertexName;
use types::HgId;

use super::Repo;
use super::Result;
use super::IO;

define_flags! {
    pub struct StatusOpts {
        #[arg]
        from: String,

        #[arg]
        to: String,
    }
}

pub fn run(opts: StatusOpts, io: &IO, repo: Repo) -> Result<u8> {
    let reponame = repo.repo_name().unwrap();
    let repopath = repo.path();
    let config = repo.config();

    let edenapi_client = edenapi::Builder::from_config(config)?.build()?;
    let namedag_path = IndexedLogNameDagPath(repopath.join(".hg/store/segments/v1"));
    let mut namedag = namedag_path
        .open()
        .context("error opening segmented changelog")?;

    let from = HgId::from_hex(opts.from.as_bytes()).unwrap();
    let to = HgId::from_hex(opts.to.as_bytes()).unwrap();
    let pull_data =
        block_on(edenapi_client.pull_fast_forward_master(reponame.to_string(), from, to))
            .context("error pulling segmented changelog")??;

    io.write(format!(
        "Got {} segments and {} ids\n",
        pull_data.flat_segments.segments.len(),
        pull_data.idmap.len()
    ))?;

    let idmap: HashMap<dag::Id, dag::Vertex> = pull_data
        .idmap
        .into_iter()
        .map(|(k, v)| (k, VertexName::copy_from(&v.into_byte_array())))
        .collect();

    let vertex_pull_data = CloneData {
        flat_segments: pull_data.flat_segments,
        idmap,
    };

    block_on(namedag.import_pull_data(vertex_pull_data))
        .context("error importing segmented changelog")??;

    let master = VertexName::copy_from(&to.into_byte_array());
    block_on(namedag.flush(&[master.clone()]))
        .context("error writing segmented changelog to disk")??;

    Ok(0)
}

pub fn name() -> &'static str {
    "debugsegmentpull"
}

pub fn doc() -> &'static str {
    "pull a repository using segmented changelog. This command does not do discovery and requrires specifying old/new master revisions"
}
