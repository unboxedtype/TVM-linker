import re
import os

def getFunctions():
	global functions
	for l in lines:
		match = re.search(r"Function (\S+)_external\s+: id=([0-9A-F]+), .*", l);
		if match:
			functions[match.group(1)] = match.group(2)
			# print match.group(1), match.group(2) 
	
def getExitCode():
	for l in lines:
		# print l
		match = re.match(r"TVM terminated with exit code (\d+)", l);
		if match:
			return int(match.group(1))
	assert False
	return -1
	
def getContractAddress():
	for l in lines:
		# print l
		match = re.match(r"Saved contract to file (.*)\.tvc", l);
		if match:
			return match.group(1)
	assert False
	return -1
	
def getStack():
	stack = []
	b = False
	for l in lines:
		if l == "--- Post-execution stack state ---------": 
			b = True
		elif l == "----------------------------------------":
			b = False
		elif b:
			ll = l.replace("Reference to ", "")
			stack.append(ll)
	return " ".join(stack)
		
def cleanup():
	if CONTRACT_ADDRESS:
		os.remove(CONTRACT_ADDRESS + ".tvc")

CONTRACT_ADDRESS = None

def compile_ex(source_file, lib_file):
	global lines, functions, CONTRACT_ADDRESS
	print("Compiling " + source_file + "...")
	lib = "--lib " + lib_file if lib_file else ""
	ec = os.system("./target/debug/tvm_linker {} ./tests/{} --debug > log".format(lib, source_file))
	assert ec == 0, ec

	lines = [l.rstrip() for l in open("log").readlines()]
	os.remove("log")

	functions = dict()
	getFunctions()
	CONTRACT_ADDRESS = getContractAddress()

SIGN = None

def exec_and_parse(method, params, expected_ec, options):
	global lines, SIGN
	sign = ("--sign " + SIGN) if SIGN else "";
	id = functions[method] if method else ""
	cmd = "./target/debug/tvm_linker {} test --body 00{}{} {} {}> log".format(CONTRACT_ADDRESS, id, params, sign, options)
	ec = os.system(cmd)
	assert ec == 0, ec

	lines = [l.rstrip() for l in open("log").readlines()]
	os.remove("log")

	ec = getExitCode()
	assert ec == expected_ec, "ec = {}".format(ec)
	
def expect_failure(method, params, expected_ec, options):
	exec_and_parse(method, params, expected_ec, options)
	print("OK:  {} {} {}".format(method, params, expected_ec))
	
def expect_success(method, params, expected, options):
	exec_and_parse(method, params, 0, options)
	stack = getStack()
	if stack != expected:
		print("Failed:  {} {}".format(method, params))
		print("EXP: ", expected)
		print("GOT: ", stack)
		quit(1)
	print("OK:  {} {} {}".format(method, params, expected))

compile_ex('test_factorial.code', 'stdlib_sol.tvm')
expect_success('constructor', "", "", "")
expect_success('main', "0003", "6", "")
expect_success('main', "0006", "726", "")
cleanup()

compile_ex('test_signature.code', 'stdlib_sol.tvm')
expect_failure('constructor', "", 100, "")
SIGN = "key1"
expect_success('constructor', "", "", "")
expect_success('get_role', "", "1", "")
SIGN = None
expect_failure('get_role', "", 100, "")
expect_failure('set_role', "", 9, "")
expect_failure('set_role', "01", 100, "")
SIGN = "key2"
expect_success('get_role', "", "0", "")
expect_success('set_role', "02", "", "")
expect_success('get_role', "", "2", "")
cleanup()

SIGN = None
compile_ex('test_inbound_int_msg.tvm', None)
expect_success(None, "", "-1", "--internal 15000000000")
cleanup()

SIGN = None
compile_ex('test_pers_data.tvm', "stdlib.tvm")
expect_success('ctor', "", "-1", "--internal 100")
cleanup()
