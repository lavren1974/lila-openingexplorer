package lila.openingexplorer

import chess.Clock

sealed abstract class SpeedGroup(
    val id: Int,
    val name: String,
    val range: Range
) {
}

object SpeedGroup {

  case object Bullet extends SpeedGroup(1, "bullet", 0 to 179)
  case object Blitz extends SpeedGroup(2, "blitz", 180 to 479)
  case object Classical extends SpeedGroup(3, "classical", 480 to Int.MaxValue)

  val all = List(Bullet, Blitz, Classical)

  val byId = all map { v => (v.id, v) } toMap

  def apply(speed: chess.Speed) = speed match {
    case chess.Speed.Bullet | chess.Speed.UltraBullet => Bullet
    case chess.Speed.Blitz => Blitz
    case chess.Speed.Classical | chess.Speed.Correspondence => Classical
  }

  def fromPgn(tags: chess.format.pgn.Tags) = tags.clockConfig.map { clock =>
    apply(chess.Speed(clock))
  } orElse {
    if (tags("TimeControl") == "-") Some(Classical) // correspondence
    else None
  }
}
