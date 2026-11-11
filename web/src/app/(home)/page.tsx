import { Site } from "@/components/home/site";

/**
 * The repository's star count, read on the server at most once an hour.
 * Null when GitHub cannot be reached.
 */
async function repositoryStars(): Promise<number | null> {
  try {
    const response = await fetch("https://api.github.com/repos/lgalabru/axel", {
      headers: { Accept: "application/vnd.github+json" },
      next: { revalidate: 3600 },
    });
    if (!response.ok) return null;
    const body: unknown = await response.json();
    const count = (body as { stargazers_count?: unknown }).stargazers_count;
    return typeof count === "number" ? count : null;
  } catch {
    return null;
  }
}

export default async function Home() {
  return <Site stars={await repositoryStars()} />;
}
