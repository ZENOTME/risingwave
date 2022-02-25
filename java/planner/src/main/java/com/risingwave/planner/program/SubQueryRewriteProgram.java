package com.risingwave.planner.program;

import static com.risingwave.planner.rules.physical.BatchRuleSets.SUB_QUERY_REWRITE_RULES;

import com.google.common.collect.Lists;
import com.risingwave.execution.context.ExecutionContext;
import com.risingwave.planner.rel.serialization.ExplainWriter;
import org.apache.calcite.plan.hep.HepMatchOrder;
import org.apache.calcite.plan.hep.HepPlanner;
import org.apache.calcite.plan.hep.HepProgram;
import org.apache.calcite.rel.RelNode;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/** The optimizer program for rewriting subqueries. */
public class SubQueryRewriteProgram implements OptimizerProgram {
  private static final Logger LOG = LoggerFactory.getLogger(SubQueryRewriteProgram.class);
  public static final SubQueryRewriteProgram INSTANCE = new SubQueryRewriteProgram();
  private static final HepProgram PROGRAM = create();

  private SubQueryRewriteProgram() {}

  @Override
  public RelNode optimize(RelNode root, ExecutionContext context) {
    var planner = new HepPlanner(PROGRAM, context);
    planner.setRoot(root);

    var ret = planner.findBestExp();

    LOG.debug("Plan after preparing subquery rewrite: \n{}", ExplainWriter.explainPlan(ret));

    //    var relBuilder = RelFactories.LOGICAL_BUILDER.create(root.getCluster(), null);
    //    return RelDecorrelator.decorrelateQuery(ret, relBuilder);
    return ret;
  }

  private static HepProgram create() {
    var builder = HepProgram.builder().addMatchOrder(HepMatchOrder.BOTTOM_UP);

    builder.addRuleCollection(Lists.newArrayList(SUB_QUERY_REWRITE_RULES.iterator()));

    return builder.build();
  }
}
