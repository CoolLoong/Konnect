//! Exact live PCB evidence shared by narrowly scoped mutation tools.

use anyhow::{ensure, Result};
use konnect_ipc::gen::kiapi;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawBoard {
    pub(crate) document: kiapi::common::types::DocumentSpecifier,
    pub(crate) items: Vec<prost_types::Any>,
    pub(crate) nets: BTreeMap<String, i32>,
}

pub(crate) fn sorted_items(mut items: Vec<prost_types::Any>) -> Vec<prost_types::Any> {
    items.sort_by(|a, b| a.type_url.cmp(&b.type_url).then(a.value.cmp(&b.value)));
    items
}

pub(crate) fn raw_board(client: &konnect_ipc::KiCadIpcClient, board: &Path) -> Result<RawBoard> {
    let document = client.find_open_board(board)?;
    // Cover the complete PCB object catalogue, including text, groups, shapes,
    // zones and generators. Retrieval failure is unavailable evidence, not zero.
    let types = (1..=17)
        .chain(std::iter::once(52))
        .map(|value| {
            kiapi::common::types::KiCadObjectType::try_from(value).expect("known PCB object type")
        })
        .collect::<Vec<_>>();
    let items = sorted_items(client.get_items_of_types_in(document.clone(), &types)?);
    let mut nets = BTreeMap::new();
    let mut codes = BTreeSet::new();
    for net in client.get_nets_in(document.clone())? {
        if net.name.is_empty() && net.netcode == 0 {
            continue;
        }
        ensure!(
            !net.name.is_empty()
                && net.netcode > 0
                && codes.insert(net.netcode)
                && nets.insert(net.name, net.netcode).is_none(),
            "ambiguous live net identity"
        );
    }
    Ok(RawBoard {
        document,
        items,
        nets,
    })
}
