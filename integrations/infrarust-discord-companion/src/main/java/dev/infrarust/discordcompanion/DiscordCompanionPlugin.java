package dev.infrarust.discordcompanion;

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;
import java.util.logging.Level;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import net.luckperms.api.LuckPerms;
import org.bukkit.NamespacedKey;
import org.bukkit.advancement.Advancement;
import org.bukkit.entity.Player;
import org.bukkit.event.EventHandler;
import org.bukkit.event.Listener;
import org.bukkit.event.entity.PlayerDeathEvent;
import org.bukkit.event.player.PlayerAdvancementDoneEvent;
import org.bukkit.plugin.RegisteredServiceProvider;
import org.bukkit.plugin.java.JavaPlugin;

/** Optional Paper backend companion; all messages are sent only to the connected proxy. */
public final class DiscordCompanionPlugin extends JavaPlugin implements Listener {
    static final String CHANNEL = "infrarust:discord";
    private static final byte VERSION = 1;
    private static final int MAX_PAYLOAD_BYTES = 512;
    private static final int MAC_BYTES = 32;
    private static final int DEATH = 1;
    private static final int ADVANCEMENT = 2;
    private static final int POSITION = 3;

    private byte[] secret;
    private boolean deaths;
    private boolean advancements;
    private boolean positions;
    private int positionIntervalTicks;
    private int maxPositionPlayers;
    private int nextPositionPlayer;
    private GroupSyncReceiver groupSync;

    @Override
    public void onEnable() {
        saveDefaultConfig();
        if (!getConfig().getBoolean("enabled", false)) {
            getLogger().info("Disabled by configuration; no channels or listeners registered.");
            return;
        }

        try {
            secret = parseSecret(getConfig().getString("shared-secret", ""));
        } catch (IllegalArgumentException exception) {
            getLogger().severe("Companion disabled: shared-secret must be exactly 64 hexadecimal characters.");
            getServer().getPluginManager().disablePlugin(this);
            return;
        }

        deaths = getConfig().getBoolean("events.death", true);
        advancements = getConfig().getBoolean("events.advancement", true);
        positions = getConfig().getBoolean("events.position", false);
        positionIntervalTicks = clamp(getConfig().getInt("position-interval-ticks", 40), 20, 1200);
        maxPositionPlayers = clamp(getConfig().getInt("max-position-players", 128), 1, 512);
        getServer().getMessenger().registerOutgoingPluginChannel(this, CHANNEL);
        getServer().getPluginManager().registerEvents(this, this);

        if (positions) {
            getServer().getScheduler().runTaskTimer(this, this::sendPositionSnapshot,
                    positionIntervalTicks, positionIntervalTicks);
        }
        getLogger().info("Enabled authenticated backend relay on " + CHANNEL + ".");
        if (getConfig().getBoolean("group-sync.enabled", false)) {
            enableGroupSync();
        }
    }

    @Override
    public void onDisable() {
        getServer().getMessenger().unregisterOutgoingPluginChannel(this, CHANNEL);
        if (groupSync != null) {
            getServer().getMessenger().unregisterIncomingPluginChannel(this, GroupSyncReceiver.CHANNEL, groupSync);
            groupSync.shutdown();
            groupSync = null;
        }
        if (secret != null) {
            java.util.Arrays.fill(secret, (byte) 0);
        }
    }

    /** Enables LuckPerms group sync. A failure here disables only group sync. */
    private void enableGroupSync() {
        String serverId = getConfig().getString("group-sync.server-id", "").trim();
        if (serverId.isEmpty() || serverId.getBytes(StandardCharsets.UTF_8).length > 128) {
            getLogger().severe("Group sync disabled: group-sync.server-id must match the proxy's backend id.");
            return;
        }
        Set<String> groups = new TreeSet<>();
        for (String configured : getConfig().getStringList("group-sync.allowed-groups")) {
            String group = configured == null ? "" : configured.trim();
            if (!GroupSyncReceiver.isValidGroup(group) || !groups.add(group)) {
                getLogger().severe("Group sync disabled: group-sync.allowed-groups has an invalid or duplicate group.");
                return;
            }
        }
        if (groups.isEmpty() || groups.size() > 16) {
            getLogger().severe("Group sync disabled: group-sync.allowed-groups needs 1 to 16 groups.");
            return;
        }
        if (getServer().getPluginManager().getPlugin("LuckPerms") == null) {
            getLogger().severe("Group sync disabled: LuckPerms is required but is not installed.");
            return;
        }
        RegisteredServiceProvider<LuckPerms> registration =
                getServer().getServicesManager().getRegistration(LuckPerms.class);
        if (registration == null) {
            getLogger().severe("Group sync disabled: LuckPerms has not registered its API.");
            return;
        }
        groupSync = new GroupSyncReceiver(this, serverId, secret, groups, registration.getProvider());
        getServer().getMessenger().registerIncomingPluginChannel(this, GroupSyncReceiver.CHANNEL, groupSync);
        getLogger().info("Enabled Discord group sync for " + groups.size() + " allowed group(s).");
    }

    @EventHandler
    public void onPlayerDeath(PlayerDeathEvent event) {
        if (deaths) {
            send(event.getEntity(), DEATH, null);
        }
    }

    @EventHandler
    public void onAdvancement(PlayerAdvancementDoneEvent event) {
        if (advancements) {
            Advancement advancement = event.getAdvancement();
            NamespacedKey key = advancement.getKey();
            send(event.getPlayer(), ADVANCEMENT, key.toString());
        }
    }

    private void sendPositionSnapshot() {
        List<Player> players = new ArrayList<>(getServer().getOnlinePlayers());
        if (players.isEmpty()) {
            nextPositionPlayer = 0;
            return;
        }
        int count = Math.min(maxPositionPlayers, players.size());
        int start = Math.floorMod(nextPositionPlayer, players.size());
        for (int i = 0; i < count; i++) {
            Player player = players.get((start + i) % players.size());
            var location = player.getLocation();
            String world = player.getWorld().getKey().toString();
            sendPosition(player, world, location.getX(), location.getY(), location.getZ());
        }
        nextPositionPlayer = (start + count) % players.size();
    }

    private void sendPosition(Player player, String world, double x, double y, double z) {
        try {
            sendEncoded(player, POSITION, out -> {
                writeString(out, world, 128);
                out.writeDouble(x);
                out.writeDouble(y);
                out.writeDouble(z);
            });
        } catch (IOException exception) {
            getLogger().log(Level.WARNING, "Could not encode position update.", exception);
        }
    }

    private void send(Player player, int type, String value) {
        try {
            sendEncoded(player, type, out -> {
                if (type == ADVANCEMENT) {
                    writeString(out, value, 128);
                }
            });
        } catch (IOException exception) {
            getLogger().log(Level.WARNING, "Could not encode backend event.", exception);
        }
    }

    private void sendEncoded(Player player, int type, PayloadWriter payloadWriter) throws IOException {
        if (secret == null || !player.isOnline()) {
            return;
        }
        ByteArrayOutputStream buffer = new ByteArrayOutputStream(160);
        try (DataOutputStream out = new DataOutputStream(buffer)) {
            out.write(new byte[]{'I', 'R', 'D', 'C'});
            out.writeByte(VERSION);
            out.writeByte(type);
            out.writeLong(System.currentTimeMillis() / 1000L);
            out.writeLong(player.getUniqueId().getMostSignificantBits());
            out.writeLong(player.getUniqueId().getLeastSignificantBits());
            payloadWriter.write(out);
        }
        byte[] body = buffer.toByteArray();
        if (body.length + MAC_BYTES > MAX_PAYLOAD_BYTES) {
            return;
        }
        byte[] mac = hmac(body);
        buffer.write(mac);
        player.sendPluginMessage(this, CHANNEL, buffer.toByteArray());
    }

    private byte[] hmac(byte[] body) {
        try {
            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec(secret, "HmacSHA256"));
            return mac.doFinal(body);
        } catch (Exception exception) {
            throw new IllegalStateException("HMAC-SHA256 is unavailable", exception);
        }
    }

    private static byte[] parseSecret(String value) {
        try {
            byte[] bytes = HexFormat.of().parseHex(value == null ? "" : value.trim());
            if (bytes.length != 32) {
                throw new IllegalArgumentException("expected 32 bytes");
            }
            return bytes;
        } catch (IllegalArgumentException exception) {
            throw new IllegalArgumentException("expected 64 hexadecimal characters", exception);
        }
    }

    private static void writeString(DataOutputStream out, String value, int maxBytes) throws IOException {
        byte[] encoded = value.getBytes(StandardCharsets.UTF_8);
        if (encoded.length == 0 || encoded.length > maxBytes) {
            throw new IOException("String exceeds wire limit");
        }
        out.writeShort(encoded.length);
        out.write(encoded);
    }

    private static int clamp(int value, int min, int max) {
        return Math.max(min, Math.min(max, value));
    }

    @FunctionalInterface
    private interface PayloadWriter {
        void write(DataOutputStream out) throws IOException;
    }
}
