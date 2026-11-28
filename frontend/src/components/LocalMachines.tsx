import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { localRequest } from "@/lib/localHost";
import { ErrorState, Spinner } from "./states";

interface Machine {
  host_id: string;
  fingerprint: string;
  label: string;
  this_machine: boolean;
  paired: boolean;
}

interface MachineList {
  this: { host_id: string; fingerprint: string; label: string };
  machines: Machine[];
  error?: string;
}

interface Spread {
  machine: string;
  outcome: string;
}

/** Pair this machine with the owner's other machines after comparing fingerprints. */
export function LocalMachines() {
  const cache = useQueryClient();
  const [compared, setCompared] = React.useState<string | null>(null);
  const list = useQuery({
    queryKey: ["local-machines"],
    queryFn: () => localRequest<MachineList>("/api/machines"),
    retry: false,
  });
  const refresh = () =>
    void cache.invalidateQueries({ queryKey: ["local-machines"] });
  const pair = useMutation({
    mutationFn: (host_id: string) =>
      localRequest<{ label: string; spread: Spread[] }>("/api/machines/pair", {
        method: "POST",
        body: JSON.stringify({ host_id, compared: true }),
      }),
    onSuccess: () => {
      setCompared(null);
      refresh();
    },
  });
  const unpair = useMutation({
    mutationFn: (host_id: string) =>
      localRequest<{ label: string; other: { told: boolean } }>(
        "/api/machines/unpair",
        { method: "POST", body: JSON.stringify({ host_id }) },
      ),
    onSuccess: refresh,
  });

  if (list.isPending) return <Spinner />;
  if (list.error) return <ErrorState message={list.error.message} />;
  const others = list.data.machines.filter((m) => !m.this_machine);
  return (
    <div className="space-y-3">
      <h2 className="text-base font-semibold">Your machines</h2>
      <p className="text-sm text-muted-foreground">
        A paired machine has full control of this one: it can start, read and
        control every session here, now and later. Pair each machine once, on
        both machines. A machine you pair also meets the machines already paired
        here.
      </p>
      <p className="text-sm">
        This machine ({list.data.this.label}):{" "}
        <code className="select-all">{list.data.this.fingerprint}</code>
      </p>
      {list.data.error && (
        <p role="alert" className="text-sm text-destructive">
          {list.data.error}
        </p>
      )}
      {others.length === 0 && (
        <p className="text-sm text-muted-foreground">
          No other machine is online on your account. Start{" "}
          <code>conversations host</code> on it.
        </p>
      )}
      <ul className="space-y-3">
        {others.map((machine) => (
          <li
            key={machine.host_id}
            className="space-y-2 rounded-lg border border-border p-3"
          >
            <div className="text-sm font-medium">
              {machine.label} {machine.paired ? "· paired" : ""}
            </div>
            <code className="block select-all text-xs">
              {machine.fingerprint}
            </code>
            {machine.paired ? (
              <Button
                variant="outline"
                disabled={unpair.isPending}
                onClick={() => unpair.mutate(machine.host_id)}
              >
                Unpair
              </Button>
            ) : (
              <>
                <label className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    checked={compared === machine.host_id}
                    onChange={(e) =>
                      setCompared(e.target.checked ? machine.host_id : null)
                    }
                  />
                  This matches the fingerprint {machine.label} shows for itself.
                </label>
                <Button
                  disabled={compared !== machine.host_id || pair.isPending}
                  onClick={() => pair.mutate(machine.host_id)}
                >
                  Pair
                </Button>
              </>
            )}
          </li>
        ))}
      </ul>
      {pair.error && (
        <p role="alert" className="text-sm text-destructive">
          {pair.error.message}
        </p>
      )}
      {pair.data && (
        <div role="status" className="text-sm">
          Paired {pair.data.label}.
          <ul>
            {pair.data.spread.map((s) => (
              <li key={s.machine}>
                {s.machine}: {s.outcome}
              </li>
            ))}
          </ul>
        </div>
      )}
      {unpair.data && (
        <p role="status" className="text-sm">
          Unpaired {unpair.data.label}
          {unpair.data.other.told
            ? "; it dropped this machine too."
            : ". It could not be told: unpair this machine there too."}
        </p>
      )}
    </div>
  );
}
