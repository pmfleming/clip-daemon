local M = {}

local function notify_failure(message)
	ya.notify({
		title = "Clipboard",
		content = message,
		level = "error",
		timeout = 5,
	})
end

local function publish(paths, operation)
	ya.async(function()
		local request, encode_error = ya.json_encode({
			op = "call",
			id = "yazi-yank",
			method = "clipboard.selection.publishFiles",
			params = { operation = operation, paths = paths },
		})
		if not request then
			return notify_failure("Could not encode the copied-file selection: " .. tostring(encode_error))
		end

		local child, spawn_error = Command("@out@/bin/clip-daemon")
			:arg("client")
			:stdin(Command.PIPED)
			:stdout(Command.PIPED)
			:stderr(Command.NULL)
			:spawn()
		if not child then
			return notify_failure("Could not contact clip-daemon: " .. tostring(spawn_error))
		end

		local written, write_error = child:write_all(request .. "\n")
		if not written then
			child:start_kill()
			return notify_failure("Could not send the copied-file selection: " .. tostring(write_error))
		end
		child:flush()

		local line, event = child:read_line_with({ timeout = 5000 })
		child:start_kill()
		if event == 3 then
			return notify_failure("clip-daemon timed out while publishing copied files")
		end
		if event ~= 0 or not line then
			return notify_failure("clip-daemon did not return a copied-file response")
		end

		local envelope, decode_error = ya.json_decode(line)
		if not envelope then
			return notify_failure("Could not decode the clip-daemon response: " .. tostring(decode_error))
		end
		if not envelope.ok or not envelope.response or not envelope.response.ok then
			local api_error = envelope.response and envelope.response.error
			local message = api_error and api_error.message or envelope.error or "unknown error"
			return notify_failure("Could not publish copied files: " .. tostring(message))
		end
	end)
end

function M:setup()
	ps.sub("@yank", function()
		local paths = {}
		for _, url in pairs(cx.yanked) do
			paths[#paths + 1] = tostring(url)
		end
		if #paths > 0 then
			publish(paths, cx.yanked.is_cut and "cut" or "copy")
		end
	end)
end

return M
