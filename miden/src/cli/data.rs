use assembly::{Library, MaslLibrary};
use miden::{
    crypto::{MerkleStore, MerkleTree, NodeIndex, PartialMerkleTree, RpoDigest, SimpleSmt},
    math::Felt,
    utils::{Deserializable, SliceReader},
    AdviceInputs, Assembler, Digest, ExecutionProof, MemAdviceProvider, Program, ProgramAst,
    StackInputs, StackOutputs, Word,
};
use serde_derive::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{stdout, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use stdlib::StdLibrary;

// HELPERS
// ================================================================================================

/// Indicates whether debug mode is on or off.
pub enum Debug {
    On,
    Off,
}

impl Debug {
    /// Returns true if debug mode is on.
    fn is_on(&self) -> bool {
        matches!(self, Self::On)
    }
}

// MERKLE DATA
// ================================================================================================

/// Struct used to deserialize merkle data from input file. Merkle data can be represented as a
/// merkle tree or a Sparse Merkle Tree.
#[derive(Deserialize, Debug)]
pub enum MerkleData {
    /// String representation of a merkle tree. The merkle tree is represented as a vector of
    /// 32 byte hex strings where each string represents a leaf in the tree.
    #[serde(rename = "merkle_tree")]
    MerkleTree(Vec<String>),
    /// String representation of a Sparse Merkle Tree. The Sparse Merkle Tree is represented as a
    /// vector of tuples where each tuple consists of a u64 node index and a 32 byte hex string
    /// representing the value of the node.
    #[serde(rename = "sparse_merkle_tree")]
    SparseMerkleTree(Vec<(u64, String)>),
    /// String representation of a Partial Merkle Tree. The Partial Merkle Tree is represented as a
    /// vector of tuples where each tuple consists of a leaf index tuple (depth, index) and a 32
    /// byte hex string representing the value of the leaf.
    #[serde(rename = "partial_merkle_tree")]
    PartialMerkleTree(Vec<((u8, u64), String)>),
}

// INPUT FILE
// ================================================================================================

// TODO consider using final types instead of string representations.
/// Input file struct that is used to deserialize input data from file. It consists of four
/// components:
/// - operand_stack
/// - advice_stack
/// - advice_map
/// - merkle_store
#[derive(Deserialize, Debug)]
pub struct InputFile {
    /// String representation of the initial operand stack, composed of chained field elements.
    pub operand_stack: Vec<String>,
    /// Optional string representation of the initial advice stack, composed of chained field
    /// elements.
    pub advice_stack: Option<Vec<String>>,
    /// Optional map of 32 byte hex strings to vectors of u64s representing the initial advice map.
    pub advice_map: Option<HashMap<String, Vec<u64>>>,
    /// Optional vector of merkle data which will be loaded into the initial merkle store. Merkle
    /// data is represented as 32 byte hex strings and node indexes are represented as u64s.
    pub merkle_store: Option<Vec<MerkleData>>,
}

/// Helper methods to interact with the input file
impl InputFile {
    pub fn read(inputs_path: &Option<PathBuf>, program_path: &Path) -> Result<Self, String> {
        // if file not specified explicitly and corresponding file with same name as program_path
        // with '.inputs' extension does't exist, set operand_stack to empty vector
        if !inputs_path.is_some() && !program_path.with_extension("inputs").exists() {
            return Ok(Self {
                operand_stack: Vec::new(),
                advice_stack: Some(Vec::new()),
                advice_map: Some(HashMap::new()),
                merkle_store: None,
            });
        }

        // If inputs_path has been provided then use this as path. Alternatively we will
        // replace the program_path extension with `.inputs` and use this as a default.
        let path = match inputs_path {
            Some(path) => path.clone(),
            None => program_path.with_extension("inputs"),
        };

        println!("Reading input file `{}`", path.display());

        // read input file to string
        let inputs_file = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to open input file `{}` - {}", path.display(), err))?;

        // deserialize input data
        let inputs: InputFile = serde_json::from_str(&inputs_file)
            .map_err(|err| format!("Failed to deserialize input data - {}", err))?;

        Ok(inputs)
    }

    /// Parse advice provider data from the input file.
    pub fn parse_advice_provider(&self) -> Result<MemAdviceProvider, String> {
        let mut advice_inputs = AdviceInputs::default();

        let stack = self
            .parse_advice_stack()
            .map_err(|e| format!("failed to parse advice provider: {e}"))?;
        advice_inputs = advice_inputs.with_stack_values(stack).map_err(|e| e.to_string())?;

        if let Some(map) = self
            .parse_advice_map()
            .map_err(|e| format!("failed to parse advice provider: {e}"))?
        {
            advice_inputs = advice_inputs.with_map(map);
        }

        if let Some(merkle_store) = self
            .parse_merkle_store()
            .map_err(|e| format!("failed to parse advice provider: {e}"))?
        {
            advice_inputs = advice_inputs.with_merkle_store(merkle_store);
        }

        Ok(MemAdviceProvider::from(advice_inputs))
    }

    /// Parse advice stack data from the input file.
    fn parse_advice_stack(&self) -> Result<Vec<u64>, String> {
        self.advice_stack
            .as_ref()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|e| format!("failed to parse advice stack value `{v}` - {e}"))
            })
            .collect::<Result<Vec<_>, _>>()
    }

    /// Parse advice map data from the input file.
    fn parse_advice_map(&self) -> Result<Option<HashMap<[u8; 32], Vec<Felt>>>, String> {
        let advice_map = match &self.advice_map {
            Some(advice_map) => advice_map,
            None => return Ok(None),
        };

        let map = advice_map
            .iter()
            .map(|(k, v)| {
                // decode hex key
                let mut key = [0u8; 32];
                hex::decode_to_slice(k, &mut key)
                    .map_err(|e| format!("failed to decode advice map key `{k}` - {e}"))?;

                // convert values to Felt
                let values = v
                    .iter()
                    .map(|v| {
                        Felt::try_from(*v).map_err(|e| {
                            format!("failed to convert advice map value `{v}` to Felt - {e}")
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((key, values))
            })
            .collect::<Result<HashMap<[u8; 32], Vec<Felt>>, String>>()?;

        Ok(Some(map))
    }

    /// Parse merkle store data from the input file.
    fn parse_merkle_store(&self) -> Result<Option<MerkleStore>, String> {
        let merkle_data = match &self.merkle_store {
            Some(merkle_data) => merkle_data,
            None => return Ok(None),
        };

        let mut merkle_store = MerkleStore::default();
        for data in merkle_data {
            match data {
                MerkleData::MerkleTree(data) => {
                    let leaves = Self::parse_merkle_tree(data)?;
                    let tree = MerkleTree::new(leaves)
                        .map_err(|e| format!("failed to parse a Merkle tree: {e}"))?;
                    merkle_store.extend(tree.inner_nodes());
                    println!("Added Merkle tree with root {} to the Merkle store", tree.root());
                }
                MerkleData::SparseMerkleTree(data) => {
                    let entries = Self::parse_sparse_merkle_tree(data)?;
                    let tree = SimpleSmt::with_leaves(u64::BITS as u8, entries)
                        .map_err(|e| format!("failed to parse a Sparse Merkle Tree: {e}"))?;
                    merkle_store.extend(tree.inner_nodes());
                    println!(
                        "Added Sparse Merkle tree with root {} to the Merkle store",
                        tree.root()
                    );
                }
                MerkleData::PartialMerkleTree(data) => {
                    let entries = Self::parse_partial_merkle_tree(data)?;
                    let tree = PartialMerkleTree::with_leaves(entries)
                        .map_err(|e| format!("failed to parse a Partial Merkle Tree: {e}"))?;
                    merkle_store.extend(tree.inner_nodes());
                    println!(
                        "Added Partial Merkle tree with root {} to the Merkle store",
                        tree.root()
                    );
                }
            }
        }

        Ok(Some(merkle_store))
    }

    /// Parse and return merkle tree leaves.
    fn parse_merkle_tree(tree: &[String]) -> Result<Vec<Word>, String> {
        tree.iter()
            .map(|v| {
                let leaf = Self::parse_word(v)?;
                Ok(leaf)
            })
            .collect()
    }

    /// Parse and return Sparse Merkle Tree entries.
    fn parse_sparse_merkle_tree(tree: &[(u64, String)]) -> Result<Vec<(u64, Word)>, String> {
        tree.iter()
            .map(|(index, v)| {
                let leaf = Self::parse_word(v)?;
                Ok((*index, leaf))
            })
            .collect()
    }

    /// Parse and return Partial Merkle Tree entries.
    fn parse_partial_merkle_tree(
        tree: &[((u8, u64), String)],
    ) -> Result<Vec<(NodeIndex, RpoDigest)>, String> {
        tree.iter()
            .map(|((depth, index), v)| {
                let node_index = NodeIndex::new(*depth, *index).map_err(|e| {
                    format!(
                        "failed to create node index with depth {depth} and index {index} - {e}"
                    )
                })?;
                let leaf = Self::parse_word(v)?;
                Ok((node_index, RpoDigest::new(leaf)))
            })
            .collect()
    }

    /// Parse a `Word` from a hex string.
    pub fn parse_word(word_hex: &str) -> Result<Word, String> {
        let word_value = &word_hex[2..];
        let mut word_data = [0u8; 32];
        hex::decode_to_slice(word_value, &mut word_data)
            .map_err(|e| format!("failed to decode `Word` from hex {word_hex} - {e}"))?;
        let mut word = Word::default();
        for (i, value) in word_data.chunks(8).enumerate() {
            word[i] = Felt::try_from(value).map_err(|e| {
                format!("failed to convert `Word` data {word_hex} (element {i}) to Felt - {e}")
            })?;
        }
        Ok(word)
    }

    /// Parse and return the stack inputs for the program.
    pub fn parse_stack_inputs(&self) -> Result<StackInputs, String> {
        let stack_inputs = self
            .operand_stack
            .iter()
            .map(|v| v.parse::<u64>().map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        StackInputs::try_from_values(stack_inputs).map_err(|e| e.to_string())
    }
}

// OUTPUT FILE
// ================================================================================================

/// Output file struct
#[derive(Deserialize, Serialize, Debug)]
pub struct OutputFile {
    pub stack: Vec<String>,
    pub overflow_addrs: Vec<String>,
}

/// Helper methods to interact with the output file
impl OutputFile {
    /// Returns a new [OutputFile] from the specified outputs vectors
    pub fn new(stack_outputs: &StackOutputs) -> Self {
        Self {
            stack: stack_outputs.stack().iter().map(|&v| v.to_string()).collect::<Vec<String>>(),
            overflow_addrs: stack_outputs
                .overflow_addrs()
                .iter()
                .map(|&v| v.to_string())
                .collect::<Vec<String>>(),
        }
    }

    /// Read the output file
    pub fn read(outputs_path: &Option<PathBuf>, program_path: &Path) -> Result<Self, String> {
        // If outputs_path has been provided then use this as path.  Alternatively we will
        // replace the program_path extension with `.outputs` and use this as a default.
        let path = match outputs_path {
            Some(path) => path.clone(),
            None => program_path.with_extension("outputs"),
        };

        println!("Reading output file `{}`", path.display());

        // read outputs file to string
        let outputs_file = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to open outputs file `{}` - {}", path.display(), err))?;

        // deserialize outputs data
        let outputs: OutputFile = serde_json::from_str(&outputs_file)
            .map_err(|err| format!("Failed to deserialize outputs data - {}", err))?;

        Ok(outputs)
    }

    /// Write the output file
    pub fn write(stack_outputs: &StackOutputs, path: &PathBuf) -> Result<(), String> {
        // if path provided, create output file
        println!("Creating output file `{}`", path.display());

        let file = fs::File::create(&path).map_err(|err| {
            format!("Failed to create output file `{}` - {}", path.display(), err)
        })?;

        println!("Writing data to output file");

        // write outputs to output file
        serde_json::to_writer_pretty(file, &Self::new(stack_outputs))
            .map_err(|err| format!("Failed to write output data - {}", err))
    }

    /// Converts outputs vectors for stack and overflow addresses to [StackOutputs].
    pub fn stack_outputs(&self) -> Result<StackOutputs, String> {
        let stack = self.stack.iter().map(|v| v.parse::<u64>().unwrap()).collect::<Vec<u64>>();

        let overflow_addrs = self
            .overflow_addrs
            .iter()
            .map(|v| v.parse::<u64>().unwrap())
            .collect::<Vec<u64>>();

        StackOutputs::new(stack, overflow_addrs)
            .map_err(|e| format!("Construct stack outputs failed {e}"))
    }
}

// PROGRAM FILE
// ================================================================================================

pub struct ProgramFile {
    ast: ProgramAst,
    path: PathBuf,
}

/// Helper methods to interact with masm program file.
impl ProgramFile {
    /// Reads the masm file at the specified path and parses it into a [ProgramAst].
    pub fn read(path: &PathBuf) -> Result<Self, String> {
        // read program file to string
        println!("Reading program file `{}`", path.display());
        let source = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to open program file `{}` - {}", path.display(), err))?;

        // parse the program into an AST
        print!("Parsing program... ");
        let now = Instant::now();
        let ast = ProgramAst::parse(&source).map_err(|err| {
            format!("Failed to parse program file `{}` - {}", path.display(), err)
        })?;
        println!("done ({} ms)", now.elapsed().as_millis());

        Ok(Self {
            ast,
            path: path.clone(),
        })
    }

    /// Compiles this program file into a [Program].
    pub fn compile<I, L>(&self, debug: &Debug, libraries: I) -> Result<Program, String>
    where
        I: IntoIterator<Item = L>,
        L: Library,
    {
        print!("Compiling program... ");
        stdout().flush().expect("Couldn't flush stdout");
        let now = Instant::now();

        // compile program
        let mut assembler = Assembler::default()
            .with_debug_mode(debug.is_on())
            .with_library(&StdLibrary::default())
            .map_err(|err| format!("Failed to load stdlib - {}", err))?;

        assembler = assembler
            .with_libraries(libraries.into_iter())
            .map_err(|err| format!("Failed to load libraries `{}`", err))?;

        let program = assembler
            .compile_ast(&self.ast)
            .map_err(|err| format!("Failed to compile program - {}", err))?;

        println!("done ({} ms)", now.elapsed().as_millis());

        Ok(program)
    }

    /// Writes this file into the specified path, if one is provided. If the path is not provided,
    /// writes the file into the same directory as the source file, but with `.masb` extension.
    pub fn write(&self, out_path: Option<PathBuf>) -> Result<(), String> {
        let out_path = out_path.unwrap_or_else(|| {
            let mut out_file = self.path.clone();
            out_file.set_extension("masb");
            out_file
        });

        self.ast
            .write_to_file(out_path)
            .map_err(|err| format!("Failed to write the compiled file: {err}"))
    }
}

// PROOF FILE
// ================================================================================================

pub struct ProofFile;

/// Helper methods to interact with proof file
impl ProofFile {
    /// Read stark proof from file
    pub fn read(
        proof_path: &Option<PathBuf>,
        program_path: &Path,
    ) -> Result<ExecutionProof, String> {
        // If proof_path has been provided then use this as path.  Alternatively we will
        // replace the program_path extension with `.proof` and use this as a default.
        let path = match proof_path {
            Some(path) => path.clone(),
            None => program_path.with_extension("proof"),
        };

        println!("Reading proof file `{}`", path.display());

        // read the file to bytes
        let file = fs::read(&path)
            .map_err(|err| format!("Failed to open proof file `{}` - {}", path.display(), err))?;

        // deserialize bytes into a stark proof
        ExecutionProof::from_bytes(&file)
            .map_err(|err| format!("Failed to decode proof data - {}", err))
    }

    /// Write stark proof to file
    pub fn write(
        proof: ExecutionProof,
        proof_path: &Option<PathBuf>,
        program_path: &Path,
    ) -> Result<(), String> {
        // If proof_path has been provided then use this as path.  Alternatively we will
        // replace the program_path extension with `.proof` and use this as a default.
        let path = match proof_path {
            Some(path) => path.clone(),
            None => program_path.with_extension("proof"),
        };

        println!("Creating proof file `{}`", path.display());

        // create output fille
        let mut file = fs::File::create(&path)
            .map_err(|err| format!("Failed to create proof file `{}` - {}", path.display(), err))?;

        let proof_bytes = proof.to_bytes();

        println!("Writing data to proof file - size {} KB", proof_bytes.len() / 1024);

        // write proof bytes to file
        file.write_all(&proof_bytes).unwrap();

        Ok(())
    }
}

// PROGRAM HASH
// ================================================================================================

pub struct ProgramHash;

/// Helper method to parse program hash from hex
impl ProgramHash {
    pub fn read(hash_hex_string: &String) -> Result<Digest, String> {
        // decode hex to bytes
        let program_hash_bytes = hex::decode(hash_hex_string)
            .map_err(|err| format!("Failed to convert program hash to bytes {}", err))?;

        // create slice reader from bytes
        let mut program_hash_slice = SliceReader::new(&program_hash_bytes);

        // create hash digest from slice
        let program_hash = Digest::read_from(&mut program_hash_slice)
            .map_err(|err| format!("Failed to deserialize program hash from bytes - {}", err))?;

        Ok(program_hash)
    }
}

// LIBRARY FILE
// ================================================================================================
pub struct Libraries {
    pub libraries: Vec<MaslLibrary>,
}

impl Libraries {
    /// Creates a new instance of [Libraries] from a list of library paths.
    pub fn new<P, I>(paths: I) -> Result<Self, String>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = P>,
    {
        let mut libraries = Vec::new();

        for path in paths {
            println!("Reading library file `{}`", path.as_ref().display());

            let library = MaslLibrary::read_from_file(path)
                .map_err(|e| format!("Failed to read library: {e}"))?;
            libraries.push(library);
        }

        Ok(Self { libraries })
    }
}

// TESTS
// ================================================================================================
#[cfg(test)]
mod test {
    use super::InputFile;

    #[test]
    fn test_merkle_data_parsing() {
        let program_with_pmt = "
        {
            \"operand_stack\": [\"1\"],
            \"merkle_store\": [
                {
                    \"partial_merkle_tree\": [
                        [
                            [2, 0],
                            \"0x1400000000000000000000000000000000000000000000000000000000000000\"
                        ],
                        [
                            [2, 1],
                            \"0x1500000000000000000000000000000000000000000000000000000000000000\"
                        ],
                        [
                            [1, 1],
                            \"0x0b00000000000000000000000000000000000000000000000000000000000000\"
                        ]
                    ]
                }
            ]
        }";
        let inputs: InputFile = serde_json::from_str(&program_with_pmt).unwrap();
        let merkle_store = inputs.parse_merkle_store().unwrap();
        assert!(merkle_store.is_some());

        let program_with_smt = "
        {
            \"operand_stack\": [\"1\"],
            \"merkle_store\": [
              {
                \"sparse_merkle_tree\": [
                  [
                    0,
                    \"0x1400000000000000000000000000000000000000000000000000000000000000\"
                  ],
                  [
                    1,
                    \"0x1500000000000000000000000000000000000000000000000000000000000000\"
                  ],
                  [
                    3,
                    \"0x1700000000000000000000000000000000000000000000000000000000000000\"
                  ]
                ]
              }
            ]
          }";
        let inputs: InputFile = serde_json::from_str(&program_with_smt).unwrap();
        let merkle_store = inputs.parse_merkle_store().unwrap();
        assert!(merkle_store.is_some());

        let program_with_merkle_tree = "
        {
            \"operand_stack\": [\"1\"],
            \"merkle_store\": [
                {
                    \"merkle_tree\": [
                        \"0x1400000000000000000000000000000000000000000000000000000000000000\",
                        \"0x1500000000000000000000000000000000000000000000000000000000000000\",
                        \"0x1600000000000000000000000000000000000000000000000000000000000000\",
                        \"0x1700000000000000000000000000000000000000000000000000000000000000\"
                    ]
                }
            ]
        }";
        let inputs: InputFile = serde_json::from_str(&program_with_merkle_tree).unwrap();
        let merkle_store = inputs.parse_merkle_store().unwrap();
        assert!(merkle_store.is_some());
    }
}
