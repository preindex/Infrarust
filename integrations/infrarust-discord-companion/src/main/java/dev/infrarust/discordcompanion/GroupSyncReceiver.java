package dev.infrarust.discordcompanion;

import java.io.File;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.MessageDigest;
import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.UUID;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.logging.Level;
import java.util.regex.Pattern;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import net.luckperms.api.LuckPerms;
import net.luckperms.api.model.user.User;
import net.luckperms.api.node.Node;
import net.luckperms.api.node.types.InheritanceNode;
import org.bukkit.configuration.file.YamlConfiguration;
import org.bukkit.entity.Player;
import org.bukkit.plugin.Plugin;
import org.bukkit.plugin.messaging.PluginMessageListener;

/**
 * Applies the Discord-derived permission groups sent by the proxy.
 *
 * <p>Only allowlisted groups are ever granted. A group is removed only when this
 * receiver granted it earlier. A manual grant of the same group cannot be
 * distinguished from the companion's grant and will be removed on revocation.
 * Ownership is persisted in {@code owned-groups.yml}. Messages must arrive on the
 * main server thread, as Bukkit plugin messages do.
 */
final class GroupSyncReceiver implements PluginMessageListener {
    static final String CHANNEL = "infrarust:discord_groups";
    private static final byte[] MAGIC = {'I', 'R', 'D', 'G'};
    private static final byte VERSION = 1;
    private static final byte SYNC = 1;
    private static final int MAC_BYTES = 32;
    private static final int MAX_PAYLOAD_BYTES = 2048;
    private static final int MAX_GROUPS = 16;
    private static final int MAX_SERVER_BYTES = 128;
    private static final int MAX_GROUP_BYTES = 64;
    /** Minimum body (magic, version, type, server, time, UUID, count) plus the MAC tag. */
    private static final int MIN_PAYLOAD_BYTES = 4 + 1 + 1 + 2 + 1 + 8 + 16 + 1 + MAC_BYTES;
    private static final Pattern GROUP_NAME = Pattern.compile("[a-z0-9][a-z0-9_.\\-]{0,63}");
    private static final long FRESHNESS_MS = 30_000L;
    private static final long FUTURE_SKEW_MS = 5_000L;
    private static final int MAX_TRACKED_PLAYERS = 8192;
    private static final int MAX_MESSAGES_PER_MINUTE = 600;
    private static final long RATE_WINDOW_MS = 60_000L;
    private static final long APPLY_TIMEOUT_SECONDS = 15L;
    private static final long SHUTDOWN_WAIT_SECONDS = 5L;
    private static final String OWNERSHIP_FILE = "owned-groups.yml";

    private final Plugin plugin;
    private final String serverId;
    private final byte[] secret;
    private final Set<String> allowedGroups;
    private final LuckPerms luckPerms;
    private final File ownershipFile;
    private final ExecutorService executor;
    /** Main thread only: last accepted timestamp per player, bounded LRU. */
    private final Map<UUID, Long> lastIssued = new LinkedHashMap<>(256, 0.75f, true) {
        @Override
        protected boolean removeEldestEntry(Map.Entry<UUID, Long> eldest) {
            return size() > MAX_TRACKED_PLAYERS;
        }
    };
    /** Main thread only: arrival times of recent messages, for the rate limit. */
    private final ArrayDeque<Long> recentMessages = new ArrayDeque<>();
    /** Executor thread only: groups this receiver granted, per player. */
    private final Map<UUID, Set<String>> owned = new HashMap<>();

    GroupSyncReceiver(Plugin plugin, String serverId, byte[] secret, Set<String> allowedGroups, LuckPerms luckPerms) {
        this.plugin = plugin;
        this.serverId = serverId;
        this.secret = secret.clone();
        this.allowedGroups = Set.copyOf(allowedGroups);
        this.luckPerms = luckPerms;
        this.ownershipFile = new File(plugin.getDataFolder(), OWNERSHIP_FILE);
        this.executor = Executors.newSingleThreadExecutor(runnable -> {
            Thread thread = new Thread(runnable, "infrarust-group-sync");
            thread.setDaemon(true);
            return thread;
        });
        loadOwnership();
    }

    /** Returns true when the name is a valid, non-default LuckPerms group. */
    static boolean isValidGroup(String group) {
        return GROUP_NAME.matcher(group).matches() && !"default".equals(group);
    }

    @Override
    public void onPluginMessageReceived(String channel, Player carrier, byte[] message) {
        if (!CHANNEL.equals(channel)) {
            return;
        }
        long now = System.currentTimeMillis();
        if (!withinRate(now)) {
            plugin.getLogger().fine("Dropped a group sync message: rate limit reached.");
            return;
        }
        Payload payload = decode(message);
        if (payload == null) {
            plugin.getLogger().fine("Rejected an unauthenticated or malformed group sync message.");
            return;
        }
        Long previous = lastIssued.get(payload.player());
        if (previous != null && payload.timestampMs() <= previous) {
            plugin.getLogger().fine("Rejected a replayed or out-of-order group sync message.");
            return;
        }
        lastIssued.put(payload.player(), payload.timestampMs());
        executor.execute(() -> apply(payload.player(), payload.groups()));
    }

    /** Stops the worker after pending work finishes and clears the secret copy. */
    void shutdown() {
        executor.shutdown();
        try {
            if (!executor.awaitTermination(SHUTDOWN_WAIT_SECONDS, TimeUnit.SECONDS)) {
                plugin.getLogger().warning("Group sync did not finish pending updates before shutdown.");
            }
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
        }
        Arrays.fill(secret, (byte) 0);
    }

    private void apply(UUID player, Set<String> desired) {
        Set<String> previous = owned.getOrDefault(player, Set.of());
        if (desired.isEmpty() && previous.isEmpty()) {
            // Common for unlinked players: nothing to grant or remove, so skip the LuckPerms load.
            return;
        }
        Set<String> nowOwned = new TreeSet<>();
        try {
            luckPerms.getUserManager().modifyUser(player, user -> {
                Set<String> present = inheritedGroups(user);
                for (String group : desired) {
                    if (!present.contains(group)) {
                        user.data().add(InheritanceNode.builder(group).build());
                        nowOwned.add(group);
                    } else if (previous.contains(group)) {
                        // Still granted by us; keep ownership. A group that was
                        // present before we granted it is not ours, so it is skipped.
                        nowOwned.add(group);
                    }
                }
                for (String group : previous) {
                    if (!desired.contains(group) && present.contains(group)) {
                        user.data().remove(InheritanceNode.builder(group).build());
                    }
                }
            }).get(APPLY_TIMEOUT_SECONDS, TimeUnit.SECONDS);
        } catch (InterruptedException exception) {
            Thread.currentThread().interrupt();
            plugin.getLogger().warning("Group sync was interrupted; a player's groups may be stale until the next sync.");
            return;
        } catch (ExecutionException | TimeoutException | RuntimeException exception) {
            plugin.getLogger().log(Level.WARNING, "Could not apply a Discord group sync; LuckPerms may be unavailable.", exception);
            return;
        }
        if (nowOwned.isEmpty()) {
            owned.remove(player);
        } else {
            owned.put(player, nowOwned);
        }
        if (!nowOwned.equals(previous)) {
            saveOwnership();
        }
    }

    private Payload decode(byte[] data) {
        if (data.length < MIN_PAYLOAD_BYTES || data.length > MAX_PAYLOAD_BYTES) {
            return null;
        }
        int bodyLength = data.length - MAC_BYTES;
        if (!authenticated(data, bodyLength)) {
            return null;
        }
        ByteBuffer input = ByteBuffer.wrap(data, 0, bodyLength);
        byte[] magic = new byte[MAGIC.length];
        input.get(magic);
        if (!Arrays.equals(magic, MAGIC) || input.get() != VERSION || input.get() != SYNC) {
            return null;
        }
        String server = readString(input, MAX_SERVER_BYTES);
        if (server == null || !server.equals(serverId)) {
            return null;
        }
        if (input.remaining() < 8 + 16 + 1) {
            return null;
        }
        long timestampMs = input.getLong();
        if (!timestampIsFresh(timestampMs)) {
            return null;
        }
        long mostSignificant = input.getLong();
        long leastSignificant = input.getLong();
        UUID player = new UUID(mostSignificant, leastSignificant);
        int count = input.get() & 0xFF;
        if (count > MAX_GROUPS) {
            return null;
        }
        Set<String> groups = new TreeSet<>();
        for (int i = 0; i < count; i++) {
            String group = readString(input, MAX_GROUP_BYTES);
            if (group == null || !allowedGroups.contains(group) || !groups.add(group)) {
                return null;
            }
        }
        if (input.hasRemaining()) {
            return null;
        }
        return new Payload(player, timestampMs, Set.copyOf(groups));
    }

    private boolean authenticated(byte[] data, int bodyLength) {
        try {
            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec(secret, "HmacSHA256"));
            mac.update(data, 0, bodyLength);
            byte[] expected = mac.doFinal();
            return MessageDigest.isEqual(expected, Arrays.copyOfRange(data, bodyLength, data.length));
        } catch (GeneralSecurityException exception) {
            return false;
        }
    }

    private boolean withinRate(long now) {
        long cutoff = now - RATE_WINDOW_MS;
        while (!recentMessages.isEmpty() && recentMessages.peekFirst() <= cutoff) {
            recentMessages.pollFirst();
        }
        if (recentMessages.size() >= MAX_MESSAGES_PER_MINUTE) {
            return false;
        }
        recentMessages.addLast(now);
        return true;
    }

    private static boolean timestampIsFresh(long timestampMs) {
        long now = System.currentTimeMillis();
        return timestampMs >= now - FRESHNESS_MS && timestampMs <= now + FUTURE_SKEW_MS;
    }

    private static String readString(ByteBuffer input, int maxBytes) {
        if (input.remaining() < 2) {
            return null;
        }
        int length = input.getShort() & 0xFFFF;
        if (length == 0 || length > maxBytes || input.remaining() < length) {
            return null;
        }
        byte[] bytes = new byte[length];
        input.get(bytes);
        return new String(bytes, StandardCharsets.UTF_8);
    }

    /** Groups granted directly, excluding contextual and temporary nodes. */
    private static Set<String> inheritedGroups(User user) {
        Set<String> groups = new HashSet<>();
        for (Node node : user.getNodes()) {
            if (node instanceof InheritanceNode inheritance && node.getContexts().isEmpty()) {
                groups.add(inheritance.getGroupName().toLowerCase(Locale.ROOT));
            }
        }
        return groups;
    }

    private void loadOwnership() {
        if (!ownershipFile.isFile()) {
            return;
        }
        YamlConfiguration yaml = YamlConfiguration.loadConfiguration(ownershipFile);
        for (String key : yaml.getKeys(false)) {
            UUID player;
            try {
                player = UUID.fromString(key);
            } catch (IllegalArgumentException exception) {
                continue;
            }
            Set<String> groups = new TreeSet<>();
            for (String group : yaml.getStringList(key)) {
                // Groups removed from the allowlist stop being managed and are left in place.
                if (allowedGroups.contains(group)) {
                    groups.add(group);
                }
            }
            if (!groups.isEmpty()) {
                owned.put(player, groups);
            }
        }
    }

    private void saveOwnership() {
        YamlConfiguration yaml = new YamlConfiguration();
        for (Map.Entry<UUID, Set<String>> entry : owned.entrySet()) {
            yaml.set(entry.getKey().toString(), List.copyOf(entry.getValue()));
        }
        try {
            yaml.save(ownershipFile);
        } catch (IOException exception) {
            // Losing this file is fail-safe: unowned groups are never removed.
            plugin.getLogger().log(Level.WARNING, "Could not save group sync ownership state.", exception);
        }
    }

    private record Payload(UUID player, long timestampMs, Set<String> groups) {
    }
}
