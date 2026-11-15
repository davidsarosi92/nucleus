// The Java nucleus codegen makes (scripts/driver-tests.sh codegen): it
// compiles (-Xlint:all -Werror) and its records take the rows.
import io.github.davidsarosi92.nucleus.Config;
import io.github.davidsarosi92.nucleus.NucleusClient;
import java.math.BigDecimal;
import java.util.Arrays;
import java.util.List;

public final class Check {
    private Check() {}

    static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError(what);
    }

    public static void main(String[] args) throws Exception {
        Config config = new Config().port(Integer.parseInt(System.getenv("NUCLEUS_PORT")));
        try (NucleusClient db = NucleusClient.connect(config)) {
            List<Schema.CgEvery> rows = db.query(Schema.CgEvery.SELECT + " ORDER BY id").rows().stream()
                    .map(Schema.CgEvery::fromRow).toList();
            Schema.CgEvery a = rows.get(0), b = rows.get(1);
            check(a.id() == 1L && a.b() && a.s() == 2 && a.i() == 3, "integers");
            check(a.r() == 1.5f && a.d() == 2.5, "floats");
            check(a.n().compareTo(new BigDecimal("12.34")) == 0, "numeric");
            check(a.m().equals("happy") && a.c().equals("abc"), "text");
            check(a.bin().length == 2 && a.dt().getYear() == 2024 && a.ts().getHour() == 3, "bytes, dates");
            check(a.tz().getOffset().getTotalSeconds() == 0 && a.tm().getSecond() == 12, "times");
            check(a.u().toString().equals("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"), "uuid");
            check(Arrays.equals(a.vec(), new float[] {1, 2, 3}), "vector");
            check(a.ia().equals(Arrays.asList(1, null, 3)) && a.ta().equals(List.of("a", "b")), "arrays");
            check(a.iv() != null && a.ip() != null && a.net() != null && a.j() != null && a.sv() != null, "others");
            check(a.select().equals("sel") && a.Two_Words().equals("tw"), "names");
            check(b.i() == 7 && b.n() == null && b.ia() == null, "nulls");
            check(db.query(Schema.CgEvents.SELECT).rows().stream().map(Schema.CgEvents::fromRow)
                    .toList().get(0).kind().equals("start"), "partitioned");
        }
        System.out.println("codegen java: ok");
    }
}
