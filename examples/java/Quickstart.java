// nucleus from Java: connect, create, insert with parameters, query, a
// transaction.  Run a server first (`nucleus server ./mydb`), then:
//
//   javac -d out $(find drivers/java/src/main examples/java -name '*.java')
//   java -cp out Quickstart

import io.github.davidsarosi92.nucleus.Config;
import io.github.davidsarosi92.nucleus.NucleusClient;
import io.github.davidsarosi92.nucleus.NucleusException;
import java.math.BigDecimal;
import java.time.LocalDate;
import java.util.List;
import java.util.Objects;

public class Quickstart {
    public static void main(String[] args) {
        // The defaults reach a local server: 127.0.0.1:20035, user/password nucleus.
        Config config = new Config().port(Integer.parseInt(
                Objects.requireNonNullElse(System.getenv("NUCLEUS_PORT"), "20035")));
        try (NucleusClient db = NucleusClient.connect(config)) {
            System.out.println("connected to " + db.serverVersion());
            db.query("DROP TABLE IF EXISTS java_orders");
            db.query("CREATE TABLE java_orders (id INT PRIMARY KEY, customer TEXT NOT NULL,"
                    + " total NUMERIC(10, 2), placed DATE, tags TEXT[])");
            db.query("INSERT INTO java_orders VALUES ($1, $2, $3, $4, $5)",
                    1, "Anna", new BigDecimal("19.99"), LocalDate.of(2024, 5, 1), List.of("new"));
            db.query("INSERT INTO java_orders VALUES ($1, $2, $3, $4, $5)",
                    2, "Béla", new BigDecimal("120.00"), LocalDate.of(2024, 5, 3), List.of("vip", "repeat"));

            // A transaction: both statements or neither.
            db.transaction(tx -> {
                tx.query("UPDATE java_orders SET total = total * 0.9 WHERE customer = $1", "Béla");
                return tx.query("INSERT INTO java_orders (id, customer, total) VALUES ($1, $2, $3)",
                        3, "Csilla", new BigDecimal("5.50"));
            });

            for (var record : db.query("SELECT id, customer, total, tags FROM java_orders ORDER BY id").records()) {
                System.out.println(record);
            }
            Object sum = db.query("SELECT sum(total) FROM java_orders").rows().get(0).get(0);
            System.out.println("sum of totals: " + sum + " (" + sum.getClass().getSimpleName() + ")");

            try {
                db.query("INSERT INTO java_orders (id, customer) VALUES (1, 'dup')");
            } catch (NucleusException e) {
                System.out.println("as expected: " + e.code() + " " + e.getMessage());
            }
            db.query("DROP TABLE java_orders");
        }
    }
}
