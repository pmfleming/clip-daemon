-- Exercise the installed plugin without publishing to the real clipboard.
local plugin_path, daemon_path = assert(arg[1]), assert(arg[2])
local callback, request, written, notified, killed, spawned, scenario
local child = {}
function child:write_all(value)
	written = value
	if scenario == "write" then return nil, "write failed" end
	return true
end
function child:flush() end
function child:start_kill() killed = true end
function child:read_line_with(options)
	assert(options.timeout == 5000)
	if scenario == "timeout" then return nil, 3 end
	if scenario == "eof" then return nil, 1 end
	return "response", 0
end
local command = {}
function command:arg(value) assert(value == "client"); return self end
function command:stdin(value) assert(value == "piped"); return self end
function command:stdout(value) assert(value == "piped"); return self end
function command:stderr(value) assert(value == "null"); return self end
function command:spawn()
	spawned = true
	if scenario == "spawn" then return nil, "spawn failed" end
	return child
end
Command = setmetatable({ PIPED = "piped", NULL = "null" }, {
	__call = function(_, path)
		assert(path == daemon_path, "plugin must use its packaged daemon")
		return command
	end,
})
ya = {
	async = function(fn) fn() end,
	notify = function(value)
		assert(value.level == "error" and value.title == "Clipboard")
		assert(type(value.content) == "string" and #value.content > 0)
		notified = true
	end,
	json_encode = function(value)
		request = value
		if scenario == "encode" then return nil, "encode failed" end
		return "request"
	end,
	json_decode = function(value)
		assert(value == "response")
		if scenario == "decode" then return nil, "decode failed" end
		if scenario == "transport" then return { ok = false, error = "unavailable" } end
		if scenario == "api" then
			return { ok = true, response = { ok = false, error = { message = "rejected" } } }
		end
		return { ok = true, response = { ok = true } }
	end,
}
ps = { sub = function(event, fn) assert(event == "@yank"); callback = fn end }
assert(loadfile(plugin_path))():setup()
assert(type(callback) == "function")
local function run(mode, cut, paths)
	scenario = mode
	request, written, notified, killed, spawned = nil, nil, false, false, false
	cx = { yanked = setmetatable({ is_cut = cut }, {
		__pairs = function() return ipairs(paths) end,
	}) }
	callback()
end
for _, cut in ipairs({ false, true }) do
	local paths = { "/tmp/file with spaces", "/tmp/quote'and\nnewline" }
	run("ok", cut, paths)
	assert(request.op == "call" and request.id == "yazi-yank")
	assert(request.method == "clipboard.selection.publishFiles")
	assert(request.params.operation == (cut and "cut" or "copy"))
	assert(#request.params.paths == 2)
	for index, path in ipairs(paths) do assert(request.params.paths[index] == path) end
	assert(written == "request\n" and killed and not notified)
end
run("ok", false, {})
assert(not spawned and not request and not notified)
for _, failure in ipairs({ "encode", "spawn", "write", "timeout", "eof", "decode", "transport", "api" }) do
	run(failure, false, { "/tmp/file" })
	assert(notified, "missing notification: " .. failure)
	if failure ~= "encode" and failure ~= "spawn" then
		assert(killed, "leaked client: " .. failure)
	end
end
print("Yazi clipboard plugin: copy/cut, empty selection and failures passed")
