package org.jetbrains.plugins.template.services

import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.diagnostic.thisLogger
import org.jetbrains.plugins.template.model.AgentV1Input
import org.jetbrains.plugins.template.model.KnownHumanInput
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Application-level service that interacts with the autter CLI
 * to create checkpoints when AI agents make edits.
 */
@Service(Service.Level.APP)
class AutterService {

    private val logger = thisLogger()
    private val minVersion = Version(1, 0, 23)

    // Stable session ID based on when the service was initialized
    val sessionId: String = System.currentTimeMillis().toString()

    @Volatile
    private var availabilityChecked = false

    @Volatile
    private var isAvailable = false

    @Volatile
    private var cachedVersion: Version? = null

    // Cached path to autter binary once resolved
    @Volatile
    private var resolvedAutterPath: String? = null

    // Track which locations were searched (for error reporting)
    private var lastSearchedPaths: List<String> = emptyList()

    data class Version(val major: Int, val minor: Int, val patch: Int) : Comparable<Version> {
        override fun compareTo(other: Version): Int {
            return compareValuesBy(this, other, { it.major }, { it.minor }, { it.patch })
        }

        override fun toString(): String = "$major.$minor.$patch"

        companion object {
            fun parse(versionString: String): Version? {
                // Expected format: "1.0.39 (debug)" or "1.0.39"
                val versionPart = versionString.trim().split(" ").first()

                val parts = versionPart.split(".")
                if (parts.size < 3) return null

                return try {
                    Version(
                        parts[0].toInt(),
                        parts[1].toInt(),
                        parts[2].split("-", "+").first().toInt()
                    )
                } catch (e: NumberFormatException) {
                    null
                }
            }
        }
    }

    /**
     * Finds the autter binary by checking known installation locations first,
     * then falling back to PATH lookup.
     *
     * Known locations (from install.sh, install.ps1, scripts/dev.sh):
     * - Production/dev build: ~/.autter/bin/autter  (dev.sh installs here too)
     * - Nix development: ~/.autter-local-dev/gitwrap/bin/autter  (nix develop shellHook)
     *
     * @return The full path to autter if found, or null if not found
     */
    private fun findAutterBinary(): String? {
        // Return cached path if already resolved and still valid
        resolvedAutterPath?.let { path ->
            if (File(path).canExecute()) {
                return path
            }
            // Cached path no longer valid, clear it
            resolvedAutterPath = null
        }

        val homeDir = System.getProperty("user.home")
        val isWindows = System.getProperty("os.name").lowercase().contains("win")

        // Known installation locations from install.sh/install.ps1/scripts/dev.sh
        // Nix dev path checked first so nix develop users can test local builds
        val knownPaths = if (isWindows) {
            listOf(
                "$homeDir\\.autter-local-dev\\gitwrap\\bin\\autter.exe",  // Nix dev (nix develop shellHook)
                "$homeDir\\.autter\\bin\\autter.exe"                      // Production + non-Nix dev (install.ps1 / dev.sh)
            )
        } else {
            listOf(
                "$homeDir/.autter-local-dev/gitwrap/bin/autter", // Nix dev (nix develop shellHook)
                "$homeDir/.autter/bin/autter"                    // Production + non-Nix dev (install.sh / dev.sh)
            )
        }

        lastSearchedPaths = knownPaths

        // Check known locations first
        for (path in knownPaths) {
            val file = File(path)
            if (file.exists() && file.canExecute()) {
                logger.info("Found autter at known location: $path")
                resolvedAutterPath = path
                return path
            }
        }

        // Fall back to PATH lookup via shell (may work if launched from terminal)
        logger.info("autter not found in known locations, trying PATH lookup")
        return tryPathLookup()
    }

    /**
     * Attempts to find autter via PATH using the shell.
     * This may work when IntelliJ is launched from a terminal with proper PATH.
     */
    private fun tryPathLookup(): String? {
        return try {
            val isWindows = System.getProperty("os.name").lowercase().contains("win")
            val command = if (isWindows) {
                listOf("cmd", "/c", "where autter")
            } else {
                listOf("/bin/sh", "-l", "-c", "which autter")
            }

            val process = ProcessBuilder(command)
                .redirectErrorStream(true)
                .start()

            val completed = process.waitFor(5, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                return null
            }

            if (process.exitValue() == 0) {
                val path = process.inputStream.bufferedReader().readText().trim().lines().firstOrNull()
                if (path != null && File(path).canExecute()) {
                    logger.info("Found autter via PATH lookup: $path")
                    resolvedAutterPath = path
                    return path
                }
            }
            null
        } catch (e: Exception) {
            logger.warn("PATH lookup for autter failed: ${e.message}")
            null
        }
    }

    /**
     * Checks if autter CLI is installed and meets the minimum version requirement.
     */
    fun checkAvailable(): Boolean {
        if (availabilityChecked) {
            return isAvailable
        }

        synchronized(this) {
            if (availabilityChecked) {
                return isAvailable
            }

            isAvailable = checkAutterInstalled()
            availabilityChecked = true
            return isAvailable
        }
    }

    private fun checkAutterInstalled(): Boolean {
        return try {
            // First, try to find the autter binary
            val autterPath = findAutterBinary()

            if (autterPath == null) {
                val currentPath = System.getenv("PATH") ?: "PATH not set"
                logger.warn("""
                    autter not found
                    Searched locations: ${lastSearchedPaths.joinToString(", ")}
                    PATH: $currentPath

                    To fix: Install autter using one of these methods:
                    - cargo install autter
                    - curl -fsSL https://install.autter.dev | sh
                    - Or ensure autter is in your PATH
                """.trimIndent())
                TelemetryService.getInstanceOrNull()?.reportAutterNotFound(
                    exitCode = null,
                    output = null,
                    searchedPaths = lastSearchedPaths,
                    currentPath = currentPath
                )
                return false
            }

            // Run version check using the resolved path
            val command = listOf(autterPath, "version")
            val process = ProcessBuilder(command)
                .redirectErrorStream(false)
                .start()

            val completed = process.waitFor(5, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                logger.warn("autter version check timed out")
                return false
            }

            val output = process.inputStream.bufferedReader().readText().trim()
            val errorOutput = process.errorStream.bufferedReader().readText().trim()

            if (process.exitValue() != 0) {
                val currentPath = System.getenv("PATH") ?: "PATH not set"
                logger.warn("""
                    autter returned error
                    Command: ${command.joinToString(" ")}
                    Exit code: ${process.exitValue()}
                    Stdout: $output
                    Stderr: $errorOutput
                    PATH: $currentPath
                    Resolved path: $autterPath
                """.trimIndent())
                TelemetryService.getInstanceOrNull()?.reportAutterNotFound(
                    exitCode = process.exitValue(),
                    output = if (errorOutput.isNotEmpty()) errorOutput else output,
                    searchedPaths = lastSearchedPaths,
                    currentPath = currentPath
                )
                return false
            }

            val version = Version.parse(output)

            if (version == null) {
                logger.warn("Could not parse autter version from: $output")
                return false
            }

            cachedVersion = version

            if (version < minVersion) {
                logger.warn("autter version $version is below minimum required version $minVersion")
                TelemetryService.getInstanceOrNull()?.reportVersionMismatch(version.toString(), minVersion.toString())
                return false
            }

            logger.info("autter CLI available at $autterPath, version: $version")
            true
        } catch (e: Exception) {
            val currentPath = System.getenv("PATH") ?: "PATH not set"
            logger.warn("""
                autter CLI not available: ${e.message}
                Searched locations: ${lastSearchedPaths.joinToString(", ")}
                PATH: $currentPath
            """.trimIndent(), e)
            TelemetryService.getInstanceOrNull()?.captureError(e, mapOf(
                "context" to "autter_availability_check",
                "searched_paths" to lastSearchedPaths.joinToString(","),
                "current_path" to currentPath
            ))
            false
        }
    }

    /**
     * Creates a checkpoint by calling autter checkpoint agent-v1 command.
     *
     * @param input The checkpoint data to send via stdin (Human or AiAgent)
     * @param workingDirectory The working directory (git repo root) for the command
     * @return true if checkpoint was created successfully
     */
    fun checkpoint(input: AgentV1Input, workingDirectory: String): Boolean {
        if (!checkAvailable()) {
            logger.warn("Skipping checkpoint - autter not available")
            return false
        }

        val autterPath = resolvedAutterPath
        if (autterPath == null) {
            logger.warn("Skipping checkpoint - autter path not resolved")
            return false
        }

        return try {
            val jsonInput = input.toJson()
            val inputType = when (input) {
                is AgentV1Input.Human -> "human"
                is AgentV1Input.AiAgent -> "ai_agent (${input.agentName})"
            }

            logger.info("Creating checkpoint (agent-v1): $inputType")
            logger.info("Checkpoint input: $jsonInput")

            val command = listOf(autterPath, "checkpoint", "agent-v1", "--hook-input", "stdin")
            val process = ProcessBuilder(command)
                .directory(File(workingDirectory))
                .redirectErrorStream(false)
                .start()

            // Write JSON to stdin
            process.outputStream.bufferedWriter().use { writer ->
                writer.write(jsonInput)
            }

            val completed = process.waitFor(30, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                logger.warn("autter checkpoint timed out")
                TelemetryService.getInstanceOrNull()?.reportCheckpointTimeout()
                return false
            }

            val output = process.inputStream.bufferedReader().readText().trim()
            val errorOutput = process.errorStream.bufferedReader().readText().trim()
            val exitCode = process.exitValue()

            if (exitCode != 0) {
                val combinedOutput = if (errorOutput.isNotEmpty()) "$output\n$errorOutput" else output
                logger.warn("""
                    autter checkpoint failed
                    Command: ${command.joinToString(" ")}
                    Exit code: $exitCode
                    Stdout: $output
                    Stderr: $errorOutput
                """.trimIndent())
                TelemetryService.getInstanceOrNull()?.reportCheckpointFailure(exitCode, combinedOutput)
                return false
            }

            logger.info("Checkpoint created successfully ($inputType)")
            if (output.isNotEmpty()) {
                logger.info("autter output: $output")
            }
            true
        } catch (e: Exception) {
            logger.warn("Failed to create checkpoint: ${e.message}", e)
            TelemetryService.getInstanceOrNull()?.captureError(e, mapOf("context" to "checkpoint_creation"))
            false
        }
    }

    /**
     * Creates a known_human checkpoint by calling autter checkpoint known_human command.
     *
     * @param input The checkpoint data to send via stdin
     * @param workingDirectory The working directory (git repo root) for the command
     * @return true if checkpoint was created successfully
     */
    fun checkpointKnownHuman(input: KnownHumanInput, workingDirectory: String): Boolean {
        if (!checkAvailable()) {
            logger.warn("Skipping known_human checkpoint - autter not available")
            return false
        }

        val autterPath = resolvedAutterPath
        if (autterPath == null) {
            logger.warn("Skipping known_human checkpoint - autter path not resolved")
            return false
        }

        return try {
            val jsonInput = input.toJson()
            logger.info("Creating known_human checkpoint for ${input.editedFilepaths}")
            logger.info("known_human checkpoint input: $jsonInput")

            val command = listOf(autterPath, "checkpoint", "known_human", "--hook-input", "stdin")
            val process = ProcessBuilder(command)
                .directory(File(workingDirectory))
                .redirectErrorStream(false)
                .start()

            process.outputStream.bufferedWriter().use { writer ->
                writer.write(jsonInput)
            }

            val completed = process.waitFor(30, TimeUnit.SECONDS)
            if (!completed) {
                process.destroyForcibly()
                logger.warn("autter known_human checkpoint timed out")
                return false
            }

            val output = process.inputStream.bufferedReader().readText().trim()
            val errorOutput = process.errorStream.bufferedReader().readText().trim()
            val exitCode = process.exitValue()

            if (exitCode != 0) {
                logger.warn("""
                    autter known_human checkpoint failed
                    Command: ${command.joinToString(" ")}
                    Exit code: $exitCode
                    Stdout: $output
                    Stderr: $errorOutput
                """.trimIndent())
                return false
            }

            logger.info("known_human checkpoint created successfully")
            if (output.isNotEmpty()) logger.info("autter output: $output")
            true
        } catch (e: Exception) {
            logger.warn("Failed to create known_human checkpoint: ${e.message}", e)
            false
        }
    }

    /**
     * Resets the availability check, forcing a re-check on next call.
     * Useful if the user installs autter during the session.
     */
    fun resetAvailabilityCheck() {
        synchronized(this) {
            availabilityChecked = false
            cachedVersion = null
            resolvedAutterPath = null
            lastSearchedPaths = emptyList()
        }
    }

    companion object {
        fun getInstance(): AutterService = service()
    }
}
