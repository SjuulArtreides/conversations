import {
  useMutation,
  useQuery,
  useQueryClient,
  type UseMutationResult,
  type UseQueryResult,
} from "@tanstack/react-query";
import { api } from "./api";
import {
  loadBrowserKey,
  signBrowserKeyRegistration,
  toBase64,
  type BrowserKey,
} from "./hostAuth";
import { useAuth } from "./useAuth";

/** Directory publication proves possession of a public key, never host access. */
export function useBrowserPublication(): {
  key: UseQueryResult<BrowserKey>;
  registration: UseQueryResult<boolean>;
  publish: UseMutationResult<void, Error, string>;
} {
  const cache = useQueryClient();
  const { user } = useAuth();
  const key = useQuery({
    queryKey: ["browser-public-key"],
    queryFn: loadBrowserKey,
  });
  const email = user?.email;
  const registrationKey = [
    "browser-publication",
    email,
    key.data?.kid,
  ] as const;
  const registration = useQuery({
    queryKey: registrationKey,
    enabled: !!email && !!key.data,
    queryFn: async () => {
      const users = await api.userKeyDirectory(email!);
      return users.some(
        (person) =>
          person.email === email &&
          person.keys.some(
            (published) =>
              published.kid === key.data!.kid &&
              published.alg === "es256" &&
              published.public_key === toBase64(key.data!.publicRaw),
          ),
      );
    },
  });
  const publish = useMutation<void, Error, string>({
    retry: false,
    mutationFn: async (label) => {
      if (!email) throw new Error("Sign in to publish this browser's key.");
      if (!key.data)
        throw new Error("This browser's key is not available yet.");
      const challenge = await api.browserKeyChallenge(
        toBase64(key.data.publicRaw),
        label,
      );
      const signature = await signBrowserKeyRegistration(
        key.data,
        challenge,
        email,
        label,
      );
      await api.publishBrowserKey(challenge.challenge, signature);
      await cache.cancelQueries({ queryKey: registrationKey, exact: true });
      cache.setQueryData(registrationKey, true);
      await cache.invalidateQueries({ queryKey: ["user-key-directory"] });
    },
  });
  return { key, registration, publish };
}
