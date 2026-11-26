import { List, View, Text, mount, signal } from "nsh";
import { Input } from "nsh/base";
import { host } from "nsh";

const state = signal({
  messages: [],
  running: false,
  aborting: false,
  current_tool: null,
  current_tool_result: null,
  error: null,
  model: null,
});
const composer = signal("");

host.on("state", (next) => state.set(next));

function send(type, extra = {}) {
  host.post("command", { type, ...extra });
}

function MessageRow({ message, index }) {
  const user = message.role === "user";
  return (
    <View
      key={`${message.role}-${index}`}
      style={{
        padding: 12,
        margin: 6,
        borderRadius: 10,
        backgroundColor: user ? "#e9e4d8" : "#f4f0e7",
        borderWidth: user ? 0 : 1,
        borderColor: "#d6cdbd",
      }}
    >
      <Text style={{ fontSize: 11, color: "#766b5b" }}>
        {user ? "YOU" : "PIR"}
      </Text>
      <Text style={{ marginTop: 5, color: "#302b24" }}>
        {message.text || (message.streaming ? "…" : "")}
      </Text>
    </View>
  );
}

function App() {
  return (
    <View style={{ flex: 1, flexDirection: "row", backgroundColor: "#e8e0d1" }}>
      <View style={{ width: 220, padding: 18, backgroundColor: "#d8cdbb" }}>
        <Text style={{ fontSize: 24, fontWeight: 700, color: "#302b24" }}>
          pir
        </Text>
        <Text style={{ marginTop: 5, color: "#766b5b" }}>
          NATIVE DESK
        </Text>
        <View style={{ marginTop: 28, padding: 10, backgroundColor: "#eee8dc", borderRadius: 8 }}>
          <Text style={{ color: "#766b5b", fontSize: 11 }}>MODEL</Text>
          <Text style={{ marginTop: 4, color: "#302b24" }}>
            {state().model ? `${state().model.provider}/${state().model.id}` : "not selected"}
          </Text>
        </View>
        <View style={{ marginTop: 10, padding: 10, backgroundColor: "#eee8dc", borderRadius: 8 }}>
          <Text style={{ color: "#766b5b", fontSize: 11 }}>STATUS</Text>
          <Text style={{ marginTop: 4, color: "#302b24" }}>
            {state().aborting ? "stopping" : state().running ? "working" : "ready"}
          </Text>
        </View>
      </View>
      <View style={{ flex: 1, padding: 20 }}>
        <Text style={{ fontSize: 20, fontWeight: 700, color: "#302b24" }}>
          Conversation
        </Text>
        {state().error ? (
          <View style={{ marginTop: 10, padding: 10, backgroundColor: "#f4d2c8", borderRadius: 8 }}>
            <Text style={{ color: "#8a3024" }}>{state().error}</Text>
          </View>
        ) : null}
        {state().current_tool ? (
          <View style={{ marginTop: 10, padding: 10, backgroundColor: "#ded5c5", borderRadius: 8 }}>
            <Text style={{ color: "#766b5b", fontSize: 11 }}>TOOL</Text>
            <Text style={{ marginTop: 3, color: "#302b24" }}>{state().current_tool}</Text>
          </View>
        ) : null}
        <List each={state().messages} estimatedItemSize={100} style={{ flex: 1, marginTop: 12 }}>
          {(message, index) => <MessageRow message={message()} index={index} />}
        </List>
        <View style={{ flexDirection: "row", marginTop: 12, padding: 8, backgroundColor: "#f4f0e7", borderRadius: 10, borderWidth: 1, borderColor: "#c9bead" }}>
          <Input
            placeholder="Ask pir…"
            value={composer()}
            onChange={({ value }) => composer.set(value)}
            onSubmit={() => {
              const text = composer().trim();
              if (text && !state().running) {
                composer.set("");
                send("submit", { text });
              }
            }}
            style={{ flex: 1, backgroundColor: "transparent" }}
          />
          <Text
            onPress={() => {
              if (state().running) send("abort");
              else {
                const text = composer().trim();
                if (text) {
                  composer.set("");
                  send("submit", { text });
                }
              }
            }}
            style={{ padding: 10, color: "#8a5737", fontWeight: 700 }}
          >
            {state().running ? "CANCEL" : "SEND"}
          </Text>
        </View>
      </View>
    </View>
  );
}

mount(App);
