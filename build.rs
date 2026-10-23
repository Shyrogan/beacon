//! Turns `data/light.json`, written by the extractor in `extractor/`, into
//! static tables indexed by block state id.

use std::fmt::Write;
use std::{env, fs, path::Path};

use serde_json::Value;

fn main() {
    println!("cargo::rerun-if-changed=data/light.json");

    let json: Value =
        serde_json::from_str(&fs::read_to_string("data/light.json").expect("read data/light.json"))
            .expect("parse data/light.json");

    let faces = json["faces"].as_array().expect("faces array");
    let states = json["states"].as_array().expect("states array");

    let mut out = String::new();
    writeln!(out, "static FACES: [FaceMask; {}] = [", faces.len()).unwrap();
    for face in faces {
        let hex = face.as_str().expect("face mask string");
        let words: Vec<String> = (0..4)
            .map(|i| format!("0x{}", &hex[i * 16..i * 16 + 16]))
            .collect();
        writeln!(out, "    FaceMask([{}]),", words.join(", ")).unwrap();
    }
    writeln!(out, "];").unwrap();

    writeln!(out, "static STATES: [StateLight; {}] = [", states.len()).unwrap();
    for state in states {
        let fields: Vec<u64> = state
            .as_array()
            .expect("state entry")
            .iter()
            .map(|v| v.as_u64().expect("state field"))
            .collect();
        let [dampening, flags, emission, faces @ ..] = fields.as_slice() else {
            panic!("state entry too short");
        };
        writeln!(
            out,
            "    StateLight {{ dampening: {dampening}, flags: {flags}, emission: {emission}, faces: {faces:?} }},"
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();

    let dest = Path::new(&env::var("OUT_DIR").unwrap()).join("light_tables.rs");
    fs::write(dest, out).unwrap();
}
