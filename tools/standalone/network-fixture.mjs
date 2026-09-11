import { createServer } from "node:http";

let networkRequests = 0;
let crossOriginRequests = 0;

const crossOriginServer = createServer((request, response) => {
  crossOriginRequests += 1;
  response.end("must-not-be-received");
});
const networkServer = createServer((request, response) => {
  if (request.url === "/__counts") {
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify({ networkRequests, crossOriginRequests }));
    return;
  }
  networkRequests += 1;
  if (request.url === "/redirect") {
    response.writeHead(302, { Location: "/ok" });
    response.end();
    return;
  }
  if (request.url === "/cross-redirect") {
    response.writeHead(302, {
      Location: `http://127.0.0.1:${crossOriginServer.address().port}/secret`,
    });
    response.end();
    return;
  }
  if (request.url === "/large") {
    response.end("x".repeat(128));
    return;
  }
  response.end("network-ok");
});

function listen(server) {
  return new Promise((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", rejectListen);
      resolveListen();
    });
  });
}

await listen(crossOriginServer);
await listen(networkServer);
process.stdout.write(`${JSON.stringify({ port: networkServer.address().port })}\n`);

function close() {
  networkServer.close(() => crossOriginServer.close(() => process.exit(0)));
}
process.once("SIGTERM", close);
process.once("SIGINT", close);
process.stdin.resume();
