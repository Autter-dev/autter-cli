// @i-know-the-amp-plugin-api-is-wip-and-very-experimental-right-now
// Required by Amp: this exact marker must remain the first line in the plugin file.
import type { PluginAPI, ToolCallEvent } from '@ampcode/plugin'
import { spawn } from 'child_process'
import { dirname, isAbsolute, resolve } from 'path'
import { fileURLToPath } from 'url'

// Absolute path to autter binary, replaced at install time by `autter install-hooks`
const AUTTER_BIN = '__AUTTER_BINARY_PATH__'

type PendingToolCall = {
	cwd: string
	tool: string
	toolInput: Record<string, unknown>
	editedFilepaths: string[]
}

function uniqueNonEmptyPaths(paths: string[]): string[] {
	const deduped = new Set<string>()
	for (const path of paths) {
		const trimmed = path.trim()
		if (trimmed) {
			deduped.add(trimmed)
		}
	}
	return Array.from(deduped)
}

function pathFromFileUri(uri: string): string | null {
	try {
		const parsed = new URL(uri)
		if (parsed.protocol !== 'file:') {
			return null
		}
		return fileURLToPath(parsed)
	} catch {
		return null
	}
}

function filesFromInput(input: Record<string, unknown>): string[] {
	const paths: string[] = []
	for (const key of ['path', 'filePath', 'file_path']) {
		const value = input[key]
		if (typeof value === 'string') {
			paths.push(value)
		}
	}

	for (const key of ['paths', 'filePaths', 'filepaths']) {
		const value = input[key]
		if (!Array.isArray(value)) {
			continue
		}
		for (const item of value) {
			if (typeof item === 'string') {
				paths.push(item)
			}
		}
	}

	return uniqueNonEmptyPaths(paths)
}

function filesFromToolCall(amp: PluginAPI, event: ToolCallEvent): string[] {
	const fromHelper = amp.helpers.filesModifiedByToolCall(event)
	const helperPaths =
		fromHelper
			?.map((uri) => pathFromFileUri(uri.toString()))
			.filter((value): value is string => value !== null) ?? []

	if (helperPaths.length > 0) {
		return uniqueNonEmptyPaths(helperPaths)
	}

	return filesFromInput(event.input)
}

export default function ampAutterPlugin(amp: PluginAPI) {
	const pendingCalls = new Map<string, PendingToolCall>()
	let autterInstalledPromise: Promise<boolean> | null = null

	const runProcess = (
		command: string,
		args: string[],
		options?: { stdin?: string },
	): Promise<{ exitCode: number; stdout: string; stderr: string }> => {
		return new Promise((resolve, reject) => {
			const child = spawn(command, args, {
				stdio: ['pipe', 'pipe', 'pipe'],
			})

			let stdout = ''
			let stderr = ''

			child.stdout.on('data', (chunk: unknown) => {
				stdout += chunk.toString()
			})
			child.stderr.on('data', (chunk: unknown) => {
				stderr += chunk.toString()
			})
			child.on('error', reject)
			child.on('close', (code) => {
				resolve({
					exitCode: code ?? -1,
					stdout,
					stderr,
				})
			})

			if (typeof options?.stdin === 'string') {
				child.stdin.write(options.stdin)
			}
			child.stdin.end()
		})
	}

	const detectGitRoot = async (args: string[]): Promise<string | null> => {
		try {
			const result = await runProcess('git', args)
			if (result.exitCode !== 0) {
				return null
			}

			const root = result.stdout.trim()
			return root || null
		} catch {
			return null
		}
	}

	const ensureAutterInstalled = async () => {
		if (!autterInstalledPromise) {
			autterInstalledPromise = (async () => {
				try {
					const result = await runProcess(AUTTER_BIN, ['--version'])
					return result.exitCode === 0
				} catch {
					return false
				}
			})()
		}
		return autterInstalledPromise
	}

	const resolveRepoRoot = async (filepaths: string[]): Promise<string> => {
		const fromCurrentDir = await detectGitRoot(['rev-parse', '--show-toplevel'])
		if (fromCurrentDir) {
			return fromCurrentDir
		}

		const candidates = new Set<string>()
		const baseDirs = [process.cwd()]
		if (typeof process.env.PWD === 'string' && process.env.PWD.trim()) {
			baseDirs.push(process.env.PWD)
		}
		for (const dir of baseDirs) {
			candidates.add(dir)
		}
		for (const path of filepaths) {
			if (isAbsolute(path)) {
				candidates.add(dirname(path))
				continue
			}
			for (const baseDir of baseDirs) {
				candidates.add(dirname(resolve(baseDir, path)))
			}
		}

		for (const candidate of candidates) {
			const root = await detectGitRoot(['-C', candidate, 'rev-parse', '--show-toplevel'])
			if (root) {
				return root
			}
		}

		return process.cwd()
	}

	const runCheckpoint = async (ctx: { logger: PluginAPI['logger'] }, payload: Record<string, unknown>) => {
		try {
			const hookInput = JSON.stringify(payload)
			const result = await runProcess(
				AUTTER_BIN,
				['checkpoint', 'amp', '--hook-input', 'stdin'],
				{ stdin: hookInput },
			)

			const stderr = result.stderr.trim()
			const shouldLogStderr =
				result.exitCode !== 0 ||
				/Failed to find any git repositories|preset error|Error: --hook-input/i.test(stderr)
			if (shouldLogStderr) {
				ctx.logger.log(
					`[autter] checkpoint amp failed (exit=${result.exitCode})${
						stderr ? `: ${stderr}` : ''
					}`,
				)
			}
		} catch (error) {
			ctx.logger.log('[autter] checkpoint amp failed', String(error))
		}
	}

	amp.on('tool.call', async (event, ctx) => {
		const autterInstalled = await ensureAutterInstalled()
		if (!autterInstalled) {
			return { action: 'allow' as const }
		}

		const editedFilepaths = filesFromToolCall(amp, event)
		const repoCwd = await resolveRepoRoot(editedFilepaths)

		const pending: PendingToolCall = {
			cwd: repoCwd,
			tool: event.tool,
			toolInput: event.input,
			editedFilepaths,
		}
		pendingCalls.set(event.toolUseID, pending)

		await runCheckpoint(
			{ logger: ctx.logger },
			{
				hook_event_name: 'PreToolUse',
				tool_use_id: event.toolUseID,
				tool_name: pending.tool,
				tool_input: pending.toolInput,
				cwd: pending.cwd,
				...(pending.editedFilepaths.length > 0
					? { edited_filepaths: pending.editedFilepaths }
					: {}),
			},
		)

		return { action: 'allow' as const }
	})

	amp.on('tool.result', async (event, ctx) => {
		const pending = pendingCalls.get(event.toolUseID)
		if (!pending) {
			return
		}
		pendingCalls.delete(event.toolUseID)

		if (event.status !== 'done') {
			return
		}

		await runCheckpoint(
			{ logger: ctx.logger },
			{
				hook_event_name: 'PostToolUse',
				tool_use_id: event.toolUseID,
				tool_name: pending.tool,
				tool_input: pending.toolInput,
				cwd: pending.cwd,
				...(pending.editedFilepaths.length > 0
					? { edited_filepaths: pending.editedFilepaths }
					: {}),
			},
		)
	})
}
