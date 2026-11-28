import { useMutation } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { pairMyMachines, usePinnedHosts } from "@/lib/machinePairing";

/** One tap: every machine this browser trusts gets full control of the others. */
export function PairMyMachines() {
  const hosts = usePinnedHosts();
  const pair = useMutation({ mutationFn: () => pairMyMachines(hosts) });
  return (
    <div className="space-y-2">
      <h3 className="text-sm font-semibold">Pair my machines</h3>
      <p className="text-sm text-muted-foreground">
        Your machines will trust each other fully: each can start, read and
        control sessions on the others, now and later. This browser needs full
        control on every machine.
      </p>
      <Button disabled={pair.isPending} onClick={() => pair.mutate()}>
        {pair.isPending ? "Pairing…" : "Pair my machines"}
      </Button>
      {pair.error && (
        <p role="alert" className="text-sm text-destructive">
          {pair.error.message}
        </p>
      )}
      {pair.data && (
        <ul className="text-sm" aria-label="Pairing results">
          {pair.data.map((r) => (
            <li key={`${r.host}:${r.peer}`}>
              {r.error
                ? `${r.host} could not pair ${r.peer}: ${r.error}`
                : `${r.host} now trusts ${r.peer}`}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
