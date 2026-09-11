const [mode, ...args] = process.argv.slice(2);

switch (mode) {
  case "cat":
    for await (const chunk of process.stdin) {
      if (!process.stdout.write(Buffer.from(chunk))) {
        await new Promise((resolve) => process.stdout.once("drain", resolve));
      }
    }
    break;
  case "binary": {
    const chunks = [];
    for await (const chunk of process.stdin) chunks.push(Buffer.from(chunk));
    process.stdout.write(Buffer.concat(chunks));
    break;
  }
  case "emit":
    process.stdout.write(Buffer.from([0x4f, 0x55, 0x54, 0x0a]));
    process.stderr.write(Buffer.from([0x45, 0x52, 0x52, 0x0a]));
    break;
  case "status":
    process.exitCode = Number.parseInt(args[0] || "1", 10);
    break;
  case "args":
    process.stdout.write(JSON.stringify(args));
    break;
  case "env":
    process.stdout.write(process.env.TEST_ONLY || "");
    break;
  case "sleep":
    await new Promise((resolve) => setTimeout(resolve, Number(args[0] || 1000)));
    break;
  case "producer": {
    const line = Buffer.from("P\n");
    const produce = () => {
      if (!process.stdout.write(line)) {
        process.stdout.once("drain", produce);
      } else {
        setImmediate(produce);
      }
    };
    process.stdout.on("error", () => process.exit(0));
    produce();
    await new Promise(() => {});
    break;
  }
  case "dual":
    for (let i = 0; i < Number(args[0] || 256); i += 1) {
      process.stdout.write(Buffer.alloc(128, 0x53));
      process.stderr.write(Buffer.alloc(128, 0x45));
    }
    break;
  case "spam":
    process.stdout.write(Buffer.alloc(Number(args[0] || 1024), 0x58));
    break;
  default:
    process.stderr.write(`unknown fixture mode: ${mode || "<missing>"}\n`);
    process.exitCode = 2;
    break;
}
