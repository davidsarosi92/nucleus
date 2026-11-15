// The sample project's Java program (PLAN.md §M14.16 M6): the functions
// nucleus codegen made of queries.sql (Queries.java), on a server whose
// schema migrations/ made.  Run by ../run.sh.
import io.github.davidsarosi92.nucleus.Config;
import io.github.davidsarosi92.nucleus.NucleusClient;
import java.time.OffsetDateTime;
import java.util.List;

public final class App {
    private App() {}

    static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError(what);
    }

    public static void main(String[] args) {
        String port = System.getenv().getOrDefault("NUCLEUS_PORT", "20035");
        try (NucleusClient db = NucleusClient.connect(new Config().port(Integer.parseInt(port)))) {
            // A transaction: the user and their posts, or none of them.
            db.transaction(tx -> {
                Queries.CreateUserRow ann = Queries.createUser(tx, 300L, "ann@java.example", "Ann").orElseThrow();
                check(ann.plan().equals("free") && ann.joined() instanceof OffsetDateTime, "createUser");
                Queries.addPost(tx, 300L, 300L, "Hello", "The first post.");
                Queries.addPost(tx, 301L, 300L, "Again", null);
                return null;
            });
            Queries.UserByEmailRow ann = Queries.userByEmail(db, "ann@java.example").orElseThrow();
            check("Ann".equals(ann.display_name()), "userByEmail");
            check(Queries.userByEmail(db, "nobody@java.example").isEmpty(), "no user");
            check(Queries.upgrade(db, ann.id()) == 1, "upgrade");
            List<List<String>> posts = Queries.postsOnPlan(db, "pro", 10L).stream()
                    .filter(p -> p.author().equals("ann@java.example"))
                    .map(p -> java.util.Arrays.asList(p.title(), p.body()))
                    .toList();
            check(posts.equals(List.of(java.util.Arrays.asList("Again", null), List.of("Hello", "The first post."))),
                    "postsOnPlan " + posts);
            check(Queries.postCounts(db).stream()
                    .anyMatch(c -> c.email().equals("ann@java.example") && c.posts() == 2L), "postCounts");
        }
        System.out.println("migrations example (java): ok");
    }
}
