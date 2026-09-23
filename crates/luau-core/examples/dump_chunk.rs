//! Opmap coverage audit input (see tools/opmap_coverage_audit.py): dump the raw
//! (un-remapped) code words and constant kinds of every proto of every bytecode
//! file given on the command line.
//! One line per proto: file, proto index, max stack, params, constant kinds, code words.
use luau_core::parser::{parse, types::Constant};

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(chunk) = parse(&bytes) else {
            println!("ERR\t{path}");
            continue;
        };
        for (i, p) in chunk.protos.iter().enumerate() {
            let kinds: String = p
                .constants
                .iter()
                .map(|k| match k {
                    Constant::Nil => 'z',
                    Constant::Boolean(_) => 'b',
                    Constant::Number(_) => 'n',
                    Constant::String(_) => 's',
                    Constant::Import(_) => 'i',
                    Constant::Table(_) => 't',
                    Constant::Closure(_) => 'c',
                    Constant::Vector(..) => 'v',
                })
                .collect();
            let code: Vec<String> = p.code.iter().map(|w| format!("{w:08x}")).collect();
            println!(
                "P\t{path}\t{i}\t{}\t{}\t{kinds}\t{}",
                p.max_stack_size,
                p.num_params,
                code.join(",")
            );
        }
    }
}
