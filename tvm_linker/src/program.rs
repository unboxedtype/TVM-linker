/*
 * Copyright 2018-2022 TON DEV SOLUTIONS LTD.
 *
 * Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
 * this file except in compliance with the License.
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific TON DEV software governing permissions and
 * limitations under the License.
 */
use base64::encode;
use crc16::*;
use ed25519_dalek::*;
use failure::bail;
use failure::format_err;
use ton_types::deserialize_cells_tree_ex;
use std::fs::File;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::collections::HashMap;
use std::time::SystemTime;
use methdict::*;
use ton_block::*;
use ton_labs_assembler::{Line, Lines, compile_code_debuggable, DbgInfo};
use ton_types::cells_serialization::{BagOfCells, deserialize_cells_tree};
use ton_types::{Cell, SliceData, BuilderData, IBitstring, Result};
use ton_types::dictionary::{HashmapE, HashmapType};
use parser::{ptr_to_builder, ParseEngine, ParseEngineResults};
use testcall::TraceLevel;

pub struct Program {
    language: Option<String>,
    engine: ParseEngineResults,
    keypair: Option<Keypair>,
    pub dbgmap: DbgInfo,
}

impl Program {
    pub fn new(parser: ParseEngine) -> Self {
        Program {
            language: None,
            engine: ParseEngineResults::new(parser),
            keypair: None,
            dbgmap: DbgInfo::new(),
        }
    }

    pub fn set_keypair(&mut self, pair: Keypair) {
        self.keypair = Some(pair);
    }

    pub fn set_language(&mut self, lang: Option<&str>) {
        self.language = lang.map(|s| s.to_owned());
    }

    pub fn data(&self) -> Result<Cell> {
        let bytes =
            if let Some(ref pair) = self.keypair {
                pair.public.to_bytes()
            } else {
                [0u8; PUBLIC_KEY_LENGTH]
            };

        // Persistent data feature is obsolete and should be removed.
        // Off-chain constructor should be used to create data layout instead.
        let (persistent_base, persistent_data) = self.engine.persistent_data();
        let mut data_dict = HashmapE::with_hashmap(64, None);
        if let Some(ref lang) = self.language {
            if lang == "C" || lang == "c" {
                data_dict = HashmapE::with_hashmap(64, persistent_data)
            }
        }
        let key = ptr_to_builder(persistent_base)?.into_cell()?.into();
        let data = BuilderData::with_raw(bytes.to_vec(), PUBLIC_KEY_LENGTH * 8)?;
        data_dict.set(key, &data.into_cell()?.into())
            .map_err(|e| format_err!("failed to pack pubkey to data dictionary: {}", e))?;
        let mut builder = BuilderData::new();
        builder
            .append_bit_one().unwrap()
            .checked_append_reference(data_dict.data().unwrap().clone()).unwrap();
        builder.into_cell()
    }

    pub fn entry(&self) -> Lines {
        self.engine.entry()
    }

    pub fn internal_method_dict(&mut self) -> Result<Option<Cell>> {
        let mut dict = prepare_methods(&self.engine.privates(), true)
            .map_err(|(i, s)| format_err!("{}", s.replace("_name_", &self.engine.global_name(i).unwrap())))?;
        self.dbgmap.append(&mut dict.1);
        Ok(dict.0.data().cloned())
    }

    fn publics_filtered(&self, remove_ctor: bool) -> HashMap<u32, Lines> {
        self.engine.publics().into_iter()
            .filter(|(k, _)|
                !(remove_ctor && self.engine.global_name(*k).unwrap_or_default() == "constructor")
            ).collect()
    }

    pub fn public_method_dict(&mut self, remove_ctor: bool) -> Result<Option<Cell>> {
        let mut dict = prepare_methods(&self.engine.internals(), true)
            .map_err(|(i, s)| format_err!("{}", s.replace("_name_", &self.engine.internal_name(i).unwrap())))?;

        insert_methods(&mut dict.0, &mut dict.1, &self.publics_filtered(remove_ctor), true)
            .map_err(|(i, s)| format_err!("{}", s.replace("_name_", &self.engine.global_name(i).unwrap())))?;

        self.dbgmap.append(&mut dict.1);

        Ok(dict.0.data().cloned())
    }

    pub fn compile_to_file_ex(
        &mut self,
        wc: i8,
        abi_file: Option<&str>,
        ctor_params: Option<&str>,
        out_file: Option<&str>,
        trace: bool,
        data_filename: Option<&str>,
    ) -> Result<String> {
        let mut state_init = self.compile_to_state()?;
        if let Some(ctor_params) = ctor_params {
            state_init = self.apply_constructor(state_init, abi_file.unwrap(), ctor_params, trace)?;
        }
        if let Some(data_filename) = data_filename {
            let mut data_cursor = Cursor::new(std::fs::read(data_filename).unwrap());
            let data_cell = deserialize_cells_tree(&mut data_cursor).unwrap().remove(0);
            state_init.set_data(data_cell);
        }
        let ret = save_to_file(state_init.clone(), out_file, wc);
        if out_file.is_some() && ret.is_ok() {
            println!("Contract successfully compiled. Saved to file {}.", out_file.unwrap());
            println!("Contract address: {:x}", state_init.hash().unwrap());
        }
        ret
    }

    fn apply_constructor(
        &mut self,
        state_init: StateInit,
        abi_file: &str,
        ctor_params : &str,
        trace: bool
    ) -> Result<StateInit> {
        let body = crate::abi::build_abi_body(
            abi_file,
            "constructor",
            ctor_params,
            None,   // header,
            None,   // key_file,
            false   // is_internal
        )?.into_cell()?.into();

        let (exit_code, mut state_init, is_vm_success) = crate::testcall::call_contract_ex(
            ton_block::MsgAddressInt::default(),
            state_init,
            None, // debug_info
            None, // balance,
            crate::testcall::MsgInfo {
                balance: None,
                src: None,
                now: get_now(),
                bounced: false,
                body: Some(body),
            },
            None, // config
            None, // key_file,
            None, // ticktock,
            None, // gas_limit,
            Some(|_, _| { }),
            if trace { TraceLevel::Full } else { TraceLevel::None },
            None  // specific capabilities
        )?;

        if is_vm_success {
            // TODO: check that no action is fired.
            // Rebuild code with removed constructor
            state_init.set_code(self.compile_asm(true)?);
            Ok(state_init)
        } else {
            bail!("Constructor failed ec = {}", exit_code)
        }
    }

    fn compile_to_state(&mut self) -> Result<StateInit> {
        let mut state = StateInit::default();
        state.set_code(self.compile_asm(false)?);
        state.set_data(self.data()?);
        Ok(state)
    }

    fn compile_asm_old(&mut self, remove_ctor: bool) -> Result<Cell> {
        let internal_selector_text = vec![
            Line::new("DICTPUSHCONST 32\n", "<internal-selector>", 1),
            Line::new("DICTUGETJMP\n",      "<internal-selector>", 2),
        ];
        let mut internal_selector = compile_code_debuggable(internal_selector_text)
            .map_err(|e| format_err!("unexpected error while compiling internal selector: {}", e))?;
        internal_selector.0.append_reference(self.internal_method_dict()?.unwrap_or_default().into());

        // adjust hash of internal_selector cell
        let hash = internal_selector.0.cell().repr_hash();
        assert_eq!(internal_selector.1.len(), 1);
        let entry = internal_selector.1.first_entry().unwrap();
        self.dbgmap.insert(hash, entry.clone());

        let (mut main_selector, main_selector_dbg) = compile_code_debuggable(self.entry())
            .map_err(|e| format_err!("unexpected error while compiling main selector: {}", e))?;
        main_selector.append_reference(self.public_method_dict(remove_ctor)?.unwrap_or_default().into());
        main_selector.append_reference(internal_selector.0);

        // adjust hash of main_selector cell
        let hash = main_selector.cell().repr_hash();
        assert_eq!(main_selector_dbg.len(), 1);
        let entry = main_selector_dbg.first_entry().unwrap();
        self.dbgmap.insert(hash, entry.clone());

        Ok(main_selector.cell().clone())
    }

    pub fn compile_asm(&mut self, remove_ctor: bool) -> Result<Cell> {
        if !self.entry().is_empty() {
            // TODO wipe out the old behavior
            return self.compile_asm_old(remove_ctor);
        }

        let internal_selector_text = vec![
            // indirect jump
            Line::new("DICTPUSHCONST 32\n", "<internal-selector>", 1),
            Line::new("DICTUGETJMP\n",      "<internal-selector>", 2),
        ];

        let mut internal_selector = compile_code_debuggable(internal_selector_text)
            .map_err(|e| format_err!("unexpected error while compiling internal selector: {}", e))?;

        let mut dict = prepare_methods(&self.engine.privates(), false)
            .map_err(|(i, s)| format_err!("{}", s.replace("_name_", &self.engine.global_name(i).unwrap())))?;

        insert_methods(&mut dict.0, &mut dict.1, &self.engine.internals(), false)
            .map_err(|(i, s)| format_err!("{}", s.replace("_name_", &self.engine.internal_name(i).unwrap())))?;

        insert_methods(&mut dict.0, &mut dict.1, &self.publics_filtered(remove_ctor), false)
            .map_err(|(i, s)| format_err!("{}", s.replace("_name_", &self.engine.global_name(i).unwrap())))?;

        let mut entry_points = vec![];
        for id in -2..1i32 {
            let key = id.serialize()?
                .into();
            let value = dict.0.remove(key).unwrap();
            entry_points.push(value.unwrap_or_default());
        }

        internal_selector.0.append_reference(SliceData::from(dict.0.data().unwrap_or(&Cell::default())));
        self.dbgmap.append(&mut dict.1);

        let version = self.engine.version();
        if let Some(version) = version {
            let version = version.as_bytes();
            internal_selector.0.append_reference(SliceData::from_raw(version.to_vec(), version.len() * 8));
        }

        // adjust hash of internal_selector cell
        let hash = internal_selector.0.cell().repr_hash();
        assert_eq!(internal_selector.1.len(), 1);
        let entry = internal_selector.1.first_entry().unwrap();
        self.dbgmap.insert(hash, entry.clone());

        let entry_selector_text = vec![
            Line::new("PUSHREFCONT\n", "<entry-selector>", 1),
            Line::new("POPCTR c3\n",   "<entry-selector>", 2),
            Line::new("DUP\n",         "<entry-selector>", 3),
            Line::new("IFNOTJMPREF\n", "<entry-selector>", 4),  //  0 - internal transaction
            Line::new("DUP\n",         "<entry-selector>", 5),
            Line::new("EQINT -1\n",    "<entry-selector>", 6),
            Line::new("IFJMPREF\n",    "<entry-selector>", 7),  // -1 - external transaction
            Line::new("DUP\n",         "<entry-selector>", 8),
            Line::new("EQINT -2\n",    "<entry-selector>", 9),
            Line::new("IFJMPREF\n",    "<entry-selector>", 10), // -2 - ticktock transaction
            Line::new("THROW 11\n",    "<entry-selector>", 11),
        ];

        let mut entry_selector = compile_code_debuggable(entry_selector_text)
            .map_err(|e| format_err!("compilation failed: {}", e))?;

        entry_selector.0.append_reference(internal_selector.0);
        entry_points.reverse();
        for entry in entry_points {
            entry_selector.0.append_reference(entry);
        }

        // adjust hash of entry_selector cell
        let hash = entry_selector.0.cell().repr_hash();
        assert_eq!(entry_selector.1.len(), 1);
        let entry = entry_selector.1.first_entry().unwrap();
        self.dbgmap.insert(hash, entry.clone());

        if !self.engine.save_my_code() {
            return Ok(entry_selector.0.cell().clone())
        }

        let save_my_code_text = vec![
            Line::new("PUSHREFCONT {\n", "<save-my-code>", 1),
            Line::new("  DUP\n",         "<save-my-code>", 2),
            Line::new("  SETGLOB 1\n",   "<save-my-code>", 3),
            Line::new("  BLESS\n",       "<save-my-code>", 4),
            Line::new("  JMPX\n",        "<save-my-code>", 5),
            Line::new("}\n",             "<save-my-code>", 6),
            Line::new("JMPXDATA\n",      "<save-my-code>", 7),
        ];
        let mut save_my_code = compile_code_debuggable(save_my_code_text)
            .map_err(|e| format_err!("compilation failed: {}", e))?;
        assert_eq!(save_my_code.1.len(), 2);
        let old_hash = save_my_code.0.cell().repr_hash();
        let entry = save_my_code.1.get(&old_hash).unwrap();
        save_my_code.0.append_reference(entry_selector.0);

        let hash = save_my_code.0.cell().repr_hash();
        self.dbgmap.insert(hash, entry.clone());

        let inner_hash = save_my_code.0.reference(0).unwrap().repr_hash();
        let entry = save_my_code.1.get(&inner_hash).unwrap();
        self.dbgmap.insert(inner_hash, entry.clone());

        Ok(save_my_code.0.cell().clone())
    }

    pub fn debug_print(&self) {
        self.engine.debug_print();
    }
}

pub fn save_to_file(state: StateInit, name: Option<&str>, wc: i8) -> Result<String> {
    let root_cell = state.write_to_new_cell()?.into_cell()?;
    let mut buffer = vec![];
    BagOfCells::with_root(&root_cell).write_to(&mut buffer, false)?;

    let mut print_filename = false;
    let address = state.hash().unwrap();
    let file_name = if let Some(name) = name {
        name.to_string()
    } else {
        print_filename = true;
        format!("{:x}.tvc", address)
    };

    let mut file = std::fs::File::create(&file_name).unwrap();
    file.write_all(&buffer)?;

    if print_filename {
        println!("Saved contract to file {}", &file_name);
        println!("testnet:");
        println!("Non-bounceable address (for init): {}", &calc_userfriendly_address(wc, address.as_slice(), false, true));
        println!("Bounceable address (for later access): {}", &calc_userfriendly_address(wc, address.as_slice(), true, true));
        println!("mainnet:");
        println!("Non-bounceable address (for init): {}", &calc_userfriendly_address(wc, address.as_slice(), false, false));
        println!("Bounceable address (for later access): {}", &calc_userfriendly_address(wc, address.as_slice(), true, false));
    }
    Ok(file_name)
}

fn calc_userfriendly_address(wc: i8, addr: &[u8], bounce: bool, testnet: bool) -> String {
    let mut bytes: Vec<u8> = vec![];
    bytes.push(if bounce { 0x11 } else { 0x51 } + if testnet { 0x80 } else { 0 });
    bytes.push(wc as u8);
    bytes.extend_from_slice(addr);
    let crc = State::<XMODEM>::calculate(&bytes);
    bytes.extend_from_slice(&crc.to_be_bytes());
    encode(&bytes)
}

pub fn load_from_file(contract_file: &str) -> Result<StateInit> {
    let mut csor = Cursor::new(std::fs::read(contract_file)?);
    let mut cell = deserialize_cells_tree(&mut csor)?.remove(0);
    // try appending a dummy library cell if there is no such cell in the tvc file
    if cell.references_count() == 2 {
        let mut adjusted_cell = BuilderData::from(cell);
        adjusted_cell.append_reference(BuilderData::default());
        cell = adjusted_cell.into_cell()?;
    }
    StateInit::construct_from_cell(cell)
}

pub fn load_stateinit(file_name: &str) -> Result<(SliceData, Vec<u8>)> {
    let mut orig_bytes = Vec::new();
    let mut f = File::open(file_name)?;
    f.read_to_end(&mut orig_bytes)?;

    let mut cur = Cursor::new(orig_bytes.clone());
    let (root_cells, _mode, _x, _y) = deserialize_cells_tree_ex(&mut cur)?;
    let mut root = root_cells[0].clone();
    if root.references_count() == 2 { // append empty library cell
        let mut adjusted_cell = BuilderData::from(root);
        adjusted_cell.append_reference(BuilderData::default());
        root = adjusted_cell.into_cell()?;
    }
    Ok((SliceData::from(root), orig_bytes))
}

pub fn get_now() -> u32 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as u32
}

#[cfg(test)]
mod tests {
    use abi;
    use std::fs::File;
    use std::path::Path;
    use crate::{printer::get_version_mycode_aware, program::load_stateinit};

    use super::*;
    use testcall::{perform_contract_call, call_contract, MsgInfo};

    fn compile_to_file(prog: &mut Program, wc: i8) -> Result<String> {
        prog.compile_to_file_ex(wc, None, None, None, false, None)
    }

    #[test]
    fn test_bouncable_address() {
        let addr = hex::decode("fcb91a3a3816d0f7b8c2c76108b8a9bc5a6b7a55bd79f8ab101c52db29232260").unwrap();
        let addr = calc_userfriendly_address(-1, &addr, true, true);
        assert_eq!(addr, "kf/8uRo6OBbQ97jCx2EIuKm8Wmt6Vb15+KsQHFLbKSMiYIny");
    }

    #[test]
    fn test_ticktock() {
        let sources = vec![Path::new("./tests/test_stdlib_sol.tvm"),
                                     Path::new("./tests/ticktock.code")];
        let parser = ParseEngine::new(sources, None);
        assert_eq!(parser.is_ok(), true);
        let mut prog = Program::new(parser.unwrap());
        let contract_file = compile_to_file(&mut prog, -1).unwrap();
        let name = contract_file.split('.').next().unwrap();

        assert_eq!(perform_contract_call(name, None, None, TraceLevel::None, false, None, Some(-1), None, None, 0, |_b,_i| {}), 0);
    }

    #[test]
    fn test_call_with_gas_limit() {
        let sources = vec![Path::new("./tests/test_stdlib_sol.tvm"),
                                     Path::new("./tests/Wallet.code")];
        let abi = abi::load_abi_json_string("./tests/Wallet.abi.json").unwrap();

        let parser = ParseEngine::new(sources, Some(abi));
        assert_eq!(parser.is_ok(), true);
        let mut prog = Program::new(parser.unwrap());

        let contract_file = compile_to_file(&mut prog, 0).unwrap();
        let name = contract_file.split('.').next().unwrap();
        let body = abi::build_abi_body("./tests/Wallet.abi.json", "constructor", "{}", None, None, false)
            .unwrap();
        let exit_code = call_contract(
            &contract_file,
            &name,
            Some("10000000000"), //account balance 10T
            MsgInfo {
                balance: Some("1000000000"), // msg balance = 1T
                src: None,
                now: 1,
                bounced: false,
                body: Some(body.into_cell().unwrap().into())
            },
            None,
            None,
            None,
            Some(3000), // gas limit
            Some(|_, _| {}),
            TraceLevel::None,
            String::from(""),
            None
        );
        // must equal to out of gas exception
        assert!(exit_code.is_ok());
        assert_eq!(exit_code.unwrap(), 13);
    }

    #[test]
    fn test_debug_map() {
        let sources = vec![Path::new("tests/test_stdlib_sol.tvm"),
                                     Path::new("tests/Wallet.code")];
        let abi = abi::load_abi_json_string("tests/Wallet.abi.json").unwrap();

        let parser = ParseEngine::new(sources, Some(abi));
        assert_eq!(parser.is_ok(), true);
        let mut prog = Program::new(parser.unwrap());

        let contract_file = compile_to_file(&mut prog, 0).unwrap();
        let debug_map_filename = String::from("tests/Wallet.map.json");
        let debug_map_file = File::create(&debug_map_filename).unwrap();
        serde_json::to_writer_pretty(debug_map_file, &prog.dbgmap).unwrap();

        let name = contract_file.split('.').next().unwrap();
        let body = abi::build_abi_body("tests/Wallet.abi.json", "constructor", "{}", None, None, false)
            .unwrap();

        let exit_code = call_contract(
            &contract_file,
            &name,
            Some("10000000000"), //account balance 10T
            MsgInfo {
                balance: Some("1000000000"), // msg balance = 1T
                src: None,
                now: 1,
                bounced: false,
                body: Some(body.into_cell().unwrap().into())
            },
            None,
            None,
            None,
            None,
            Some(|_, _| {}),
            TraceLevel::Full,
            debug_map_filename,
            None
        );
        assert!(exit_code.is_ok());
        assert_eq!(exit_code.unwrap(), 0);
    }

    fn get_version(filename: &str) -> Result<String> {
        let parser = ParseEngine::new(vec![Path::new(filename)], None);
        assert_eq!(parser.is_ok(), true);
        let mut prog = Program::new(parser.unwrap());
        let file_name = compile_to_file(&mut prog, -1).unwrap();
        let (mut root_slice, _) = load_stateinit(file_name.as_str())?;
        let state = StateInit::construct_from(&mut root_slice)?;
        get_version_mycode_aware(state.code.as_ref())
    }

    #[test]
    fn test_get_version() {
        assert_eq!(
            "0.43.0+commit.e8c3d877.mod.Linux.g++".to_string(),
            get_version("tests/get-version1.code").unwrap_or("".to_string()));
        assert_eq!(
            "0.43.0+commit.e8c3d877.mod.Linux.g++".to_string(),
            get_version("tests/get-version2.code").unwrap_or("".to_string()));
        assert_eq!(
            "not found (cell underflow)".to_string(),
            get_version("tests/get-version3.code").unwrap_err().to_string());
    }

    #[test]
    fn test_mycode() {
        let sources = vec![Path::new("tests/test_stdlib_sol.tvm"), Path::new("tests/mycode.code")];
        let abi = abi::load_abi_json_string("tests/mycode.abi.json").unwrap();

        let parser = ParseEngine::new(sources, Some(abi));
        assert_eq!(parser.is_ok(), true);
        let mut prog = Program::new(parser.unwrap());

        let contract_file = compile_to_file(&mut prog, 0).unwrap();
        let name = contract_file.split('.').next().unwrap();
        let body = abi::build_abi_body("tests/mycode.abi.json", "constructor", "{}", None, None, false)
            .unwrap();
        let exit_code = call_contract(
            &contract_file,
            &name,
            Some("10000000000"), //account balance 10T
            MsgInfo {
                balance: Some("1000000000"), // msg balance = 1T
                src: None,
                now: 1,
                bounced: false,
                body: Some(body.into_cell().unwrap().into())
            },
            None,
            None,
            None,
            None,
            Some(|_, _| {}),
            TraceLevel::None,
            String::new(),
            Some(GlobalCapabilities::CapMycode as u64)
        );
        assert!(exit_code.is_ok());
        assert_eq!(exit_code.unwrap(), 0);
    }
}
