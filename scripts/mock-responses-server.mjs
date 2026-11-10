// Mock Responses API server for testing streaming without credentials.
// Usage: node scripts/mock-responses-server.mjs [port]   (default 18800)
// Point InoCode at it:
//   -c model_provider=mock
//   -c model_providers.mock.name=mock
//   -c model_providers.mock.base_url=http://127.0.0.1:18800/v1
//   -c model_providers.mock.wire_api=responses
import http from "node:http";

const port = Number(process.argv[2] ?? 18800);
const TEXT = "これはモックモデルの応答です。文字列が少しずつ流れてきます。";
const sse = (res, event, data) =>
  res.write("event: " + event + "\ndata: " + JSON.stringify(data) + "\n\n");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

http
  .createServer(async (req, res) => {
    if (req.method === "POST" && req.url.startsWith("/v1/responses")) {
      req.resume();
      await new Promise((r) => req.on("end", r));
      res.writeHead(200, { "content-type": "text/event-stream" });
      const id = "resp_mock_1";
      const item = { id: "msg_mock_1", type: "message", role: "assistant", content: [] };
      sse(res, "response.created", { type: "response.created", response: { id } });
      sse(res, "response.output_item.added", { type: "response.output_item.added", output_index: 0, item });
      let acc = "";
      for (const ch of Array.from(TEXT)) {
        acc += ch;
        sse(res, "response.output_text.delta", { type: "response.output_text.delta", item_id: item.id, output_index: 0, content_index: 0, delta: ch });
        await sleep(40);
      }
      const done = { ...item, content: [{ type: "output_text", text: acc }] };
      sse(res, "response.output_item.done", { type: "response.output_item.done", output_index: 0, item: done });
      sse(res, "response.completed", {
        type: "response.completed",
        response: { id, usage: { input_tokens: 1, input_tokens_details: null, output_tokens: 1, output_tokens_details: null, total_tokens: 2 } },
      });
      res.end();
    } else {
      res.writeHead(404).end();
    }
  })
  .listen(port, "127.0.0.1", () => console.log("mock responses server on " + port));
