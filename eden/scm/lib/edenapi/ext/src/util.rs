/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;

use blake2::digest::Input;
use blake2::digest::VariableOutput;
use blake2::VarBlake2b;
use edenapi_types::ContentId;

pub fn calc_contentid(data: &[u8]) -> ContentId {
    let mut hash = VarBlake2b::new_keyed(b"content", ContentId::len());
    hash.input(data);
    let mut ret = [0u8; ContentId::len()];
    hash.variable_result(|res| {
        if let Err(e) = ret.as_mut().write_all(res) {
            panic!(
                "{}-byte array must work with {}-byte blake2b: {:?}",
                ContentId::len(),
                ContentId::len(),
                e
            );
        }
    });
    ContentId::from(ret)
}
